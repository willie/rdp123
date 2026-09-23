//! Rendering + modal-dialog helpers. Everything here runs on the main thread.

use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSCursor,
    NSImage, NSSecureTextField, NSView,
};
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CGFloat, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo,
};
use objc2_foundation::{NSData, NSString};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceHeight, kIOSurfacePixelFormat, kIOSurfaceWidth,
    IOSurfaceLockOptions, IOSurfaceRef,
};
use objc2_quartz_core::CALayer;

/// The menu-bar status icon: a template image so macOS tints it correctly for
/// light and dark menu bars automatically.
pub fn menu_bar_icon() -> Option<Retained<NSImage>> {
    let bytes: &[u8] = include_bytes!("../../../assets/menubar@2x.png");
    let data = NSData::with_bytes(bytes);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setSize(CGSize::new(18.0, 18.0));
    image.setTemplate(true);
    Some(image)
}

use std::sync::{Arc, Mutex};

use rdp123_core::SharedFramebuffer;

const FIRST_BUTTON: isize = 1000; // NSAlertFirstButtonReturn

/// Presentation buffers recycled between paints. Each entry carries the
/// framebuffer generation it was last synced to, so
/// [`SharedFramebuffer::present_into`] copies only the regions dirtied since.
pub type PresentPool = Arc<Mutex<Vec<(Vec<u8>, u64)>>>;

/// Buffers kept in the pool; beyond this, returned buffers are freed (only
/// relevant transiently, e.g. right after a resize).
const PRESENT_POOL_CAP: usize = 4;

struct PresentRelease {
    pool: PresentPool,
    buf: Vec<u8>,
    generation: u64,
}

/// A recycled IOSurface presentation target plus the framebuffer generation
/// it was last synced to. Main-thread only.
pub struct PresentSurface {
    surface: CFRetained<IOSurfaceRef>,
    synced_generation: u64,
}

/// Pool of IOSurfaces cycled through `CALayer.contents`.
pub type SurfacePool = std::cell::RefCell<Vec<PresentSurface>>;

/// WindowServer normally holds one or two surfaces; more than this in flight
/// means something is wrong, and the CGImage path takes over for that paint.
const SURFACE_POOL_CAP: usize = 6;

/// FourCC 'BGRA': byte order B,G,R,A — matches the framebuffer's BGRX layout
/// (the alpha byte is ignored because the layer is marked opaque).
const FOURCC_BGRA: i32 = 0x4247_5241;

fn new_iosurface(width: u16, height: u16) -> Option<CFRetained<IOSurfaceRef>> {
    let width = CFNumber::new_i32(i32::from(width));
    let height = CFNumber::new_i32(i32::from(height));
    let bytes_per_element = CFNumber::new_i32(4);
    let pixel_format = CFNumber::new_i32(FOURCC_BGRA);
    let keys: [&CFString; 4] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfaceBytesPerElement,
            kIOSurfacePixelFormat,
        ]
    };
    let values: [&CFNumber; 4] = [&width, &height, &bytes_per_element, &pixel_format];
    let properties = CFDictionary::from_slices(&keys, &values);
    // Same memory layout regardless of the dictionary's type parameters.
    let properties: &CFDictionary = unsafe {
        &*((&*properties) as *const CFDictionary<CFString, CFNumber> as *const CFDictionary)
    };
    unsafe { IOSurfaceRef::new(properties) }
}

/// Present via a pooled IOSurface: CoreAnimation binds the surface memory as
/// a texture directly, so no CGImage is built and no full-frame copy is
/// uploaded — the dirty regions synced by `present_into_stride` are all the
/// CPU work there is. Returns `false` if no surface could be used; the caller
/// should fall back to [`upload_framebuffer`].
pub fn upload_framebuffer_iosurface(
    layer: &CALayer,
    framebuffer: &SharedFramebuffer,
    pool: &SurfacePool,
) -> bool {
    let (fb_width, fb_height) = framebuffer.dimensions();
    if fb_width == 0 || fb_height == 0 {
        return false;
    }

    let mut entries = pool.borrow_mut();
    // Drop surfaces from before a resize.
    entries.retain(|entry| {
        entry.surface.width() == usize::from(fb_width)
            && entry.surface.height() == usize::from(fb_height)
    });
    // Reuse a surface the WindowServer is done with, else allocate a new one.
    let mut entry = match entries.iter().position(|entry| !entry.surface.is_in_use()) {
        Some(index) => entries.swap_remove(index),
        None if entries.len() >= SURFACE_POOL_CAP => return false,
        None => match new_iosurface(fb_width, fb_height) {
            Some(surface) => PresentSurface {
                surface,
                synced_generation: 0,
            },
            None => return false,
        },
    };
    drop(entries);

    if unsafe {
        entry
            .surface
            .lock(IOSurfaceLockOptions::empty(), core::ptr::null_mut())
    } != 0
    {
        return false;
    }
    let stride = entry.surface.bytes_per_row();
    let len = entry.surface.alloc_size();
    let base = entry.surface.base_address().as_ptr().cast::<u8>();
    // Safety: the surface is locked, giving exclusive CPU access to its
    // allocation of `len` bytes.
    let dst = unsafe { core::slice::from_raw_parts_mut(base, len) };
    let synced = framebuffer.present_into_stride(dst, stride, entry.synced_generation);
    let _ = unsafe {
        entry
            .surface
            .unlock(IOSurfaceLockOptions::empty(), core::ptr::null_mut())
    };
    let Some((_, _, generation)) = synced else {
        pool.borrow_mut().push(entry);
        return false;
    };
    entry.synced_generation = generation;

    // The alpha byte is padding (BGRX); opaque tells CA to ignore it.
    layer.setOpaque(true);
    let obj: &AnyObject =
        unsafe { &*((&*entry.surface) as *const IOSurfaceRef as *const AnyObject) };
    unsafe { layer.setContents(Some(obj)) };
    pool.borrow_mut().push(entry);
    true
}

/// Sync a pooled presentation buffer from the framebuffer (copying only dirty
/// regions) and push it into the layer as a fresh `CGImage`.
///
/// The pixel buffer is BGRX; CoreGraphics reads it as little-endian 32-bit with
/// the leading byte skipped, i.e. `[B,G,R,X] -> RGB`. The buffer is owned by
/// the image while CoreAnimation uses it; the release callback returns it to
/// the pool.
pub fn upload_framebuffer(layer: &CALayer, framebuffer: &SharedFramebuffer, pool: &PresentPool) {
    let (mut buf, synced_generation) = pool.lock().unwrap().pop().unwrap_or((Vec::new(), 0));
    let Some((width, height, generation)) = framebuffer.present_into(&mut buf, synced_generation)
    else {
        return;
    };
    if width == 0 || height == 0 {
        return;
    }

    let bytes_per_row = usize::from(width) * 4;

    let Some(color_space) = CGColorSpace::new_device_rgb() else {
        return;
    };
    let bitmap_info =
        CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);

    // The provider owns the buffer for the lifetime of the image; the release
    // callback returns it to the pool. `with_data` borrows, it does not copy.
    let ctx = Box::new(PresentRelease {
        pool: pool.clone(),
        buf,
        generation,
    });
    let data_ptr = ctx.buf.as_ptr() as *const c_void;
    let len = ctx.buf.len();
    let info = Box::into_raw(ctx) as *mut c_void;

    let provider =
        unsafe { CGDataProvider::with_data(info, data_ptr, len, Some(release_present_buffer)) };
    let Some(provider) = provider else {
        // Reclaim the leaked context if provider creation failed.
        unsafe { drop(Box::from_raw(info as *mut PresentRelease)) };
        return;
    };

    let image = unsafe {
        CGImage::new(
            usize::from(width),
            usize::from(height),
            8,
            32,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
            Some(&provider),
            core::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };
    // On failure the provider still owns the context and releases it on drop.
    let Some(image) = image else {
        return;
    };

    let obj: &AnyObject = unsafe { &*(&*image as *const CGImage as *const AnyObject) };
    unsafe { layer.setContents(Some(obj)) };
}

unsafe extern "C-unwind" fn release_present_buffer(
    info: *mut c_void,
    _data: NonNull<c_void>,
    _size: usize,
) {
    let ctx = unsafe { Box::from_raw(info as *mut PresentRelease) };
    let PresentRelease {
        pool,
        buf,
        generation,
    } = *ctx;
    // Never unwind through a CoreGraphics callback. If another panic poisoned
    // the pool, dropping this buffer is safer than crossing the FFI boundary.
    if let Ok(mut buffers) = pool.lock() {
        if buffers.len() < PRESENT_POOL_CAP {
            buffers.push((buf, generation));
        }
    };
}

unsafe extern "C-unwind" fn release_pixels(
    info: *mut c_void,
    _data: NonNull<c_void>,
    _size: usize,
) {
    drop(unsafe { Box::from_raw(info as *mut Vec<u8>) });
}

/// Build an `NSCursor` from a straight-alpha RGBA pointer bitmap.
///
/// `point_scale` converts remote pixels to view points (bounds / framebuffer
/// size) so the cursor matches the on-screen scale of the remote desktop; the
/// hotspot is scaled with it. `NSCursor` hotspots use a top-left origin, same
/// as RDP.
pub fn make_remote_cursor(
    rgba: Vec<u8>,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    point_scale: f64,
) -> Option<Retained<NSCursor>> {
    if width == 0 || height == 0 || rgba.len() < usize::from(width) * usize::from(height) * 4 {
        return None;
    }

    let bytes_per_row = usize::from(width) * 4;
    let color_space = CGColorSpace::new_device_rgb()?;
    // Byte order big + alpha last = R,G,B,A in memory, straight alpha.
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::Last.0 | CGImageByteOrderInfo::OrderDefault.0);

    let boxed = Box::new(rgba);
    let data_ptr = boxed.as_ptr() as *const c_void;
    let len = boxed.len();
    let info = Box::into_raw(boxed) as *mut c_void;

    let provider = unsafe { CGDataProvider::with_data(info, data_ptr, len, Some(release_pixels)) };
    let Some(provider) = provider else {
        unsafe { drop(Box::from_raw(info as *mut Vec<u8>)) };
        return None;
    };

    let image = unsafe {
        CGImage::new(
            usize::from(width),
            usize::from(height),
            8,
            32,
            bytes_per_row,
            Some(&color_space),
            bitmap_info,
            Some(&provider),
            core::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;

    let size = CGSize::new(
        f64::from(width) * point_scale,
        f64::from(height) * point_scale,
    );
    let ns_image = NSImage::initWithCGImage_size(NSImage::alloc(), &image, size);
    let hotspot = CGPoint::new(
        f64::from(hotspot_x) * point_scale,
        f64::from(hotspot_y) * point_scale,
    );
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &ns_image,
        hotspot,
    ))
}

/// A fully transparent cursor for the server's "pointer hidden" state.
pub fn make_hidden_cursor() -> Option<Retained<NSCursor>> {
    make_remote_cursor(vec![0u8; 4], 1, 1, 0, 0, 1.0)
}

/// Prompt for a password. Returns `(password, save_to_keychain)`, or `None` on
/// cancel. `default_save` sets the initial state of the "remember" checkbox.
pub fn prompt_password(
    mtm: MainThreadMarker,
    title: &str,
    default_save: bool,
) -> Option<(String, bool)> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(
        "Enter the password for this connection.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Connect"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    // Accessory: a secure field with a "remember" checkbox below it.
    let container = NSView::initWithFrame(
        NSView::alloc(mtm),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(280.0, 52.0)),
    );
    let field = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        CGRect::new(CGPoint::new(0.0, 28.0), CGSize::new(280.0, 24.0)),
    );
    let check = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Save password to Keychain"),
            None,
            None,
            mtm,
        )
    };
    check.setFrame(CGRect::new(
        CGPoint::new(0.0, 2.0),
        CGSize::new(280.0, 20.0),
    ));
    check.setState(if default_save {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    container.addSubview(&field);
    container.addSubview(&check);
    alert.setAccessoryView(Some(&container));
    alert.window().setInitialFirstResponder(Some(&field));

    if alert.runModal() == FIRST_BUTTON {
        let save = check.state() == NSControlStateValueOn;
        Some((field.stringValue().to_string(), save))
    } else {
        None
    }
}

/// Ask the user to trust a server key fingerprint. Returns true if accepted.
pub fn prompt_certificate(mtm: MainThreadMarker, fingerprint: &str, is_change: bool) -> bool {
    let alert = NSAlert::new(mtm);
    let title = if is_change {
        "Server key has CHANGED"
    } else {
        "Trust this server?"
    };
    let body = if is_change {
        format!(
            "The server's key fingerprint is different from the one you trusted before. \
             This could indicate a man-in-the-middle attack.\n\n{fingerprint}\n\nTrust it anyway?"
        )
    } else {
        format!("First connection to this server.\n\nKey fingerprint:\n{fingerprint}")
    };
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(&body));
    if is_change {
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str("Trust New Key"));
        alert.runModal() == 1001
    } else {
        alert.addButtonWithTitle(&NSString::from_str("Trust"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.runModal() == FIRST_BUTTON
    }
}

/// The user's choice in the "unsaved changes" alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsavedChoice {
    Save,
    Discard,
    Cancel,
}

/// Ask what to do about unsaved edits to `name`. Buttons map to 1000/1001/1002.
pub fn confirm_unsaved(mtm: MainThreadMarker, name: &str) -> UnsavedChoice {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Save changes to “{name}”?")));
    alert.setInformativeText(&NSString::from_str(
        "Your edits will be lost if you don't save them.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Save"));
    // NSAlert gives a button titled "Don't Save" its standard ⌘D shortcut.
    alert.addButtonWithTitle(&NSString::from_str("Don’t Save"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    match alert.runModal() {
        FIRST_BUTTON => UnsavedChoice::Save,
        1001 => UnsavedChoice::Discard,
        _ => UnsavedChoice::Cancel,
    }
}

pub fn confirm_delete(mtm: MainThreadMarker, name: &str) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(&format!("Delete “{name}”?")));
    alert.setInformativeText(&NSString::from_str(
        "The connection and its saved Keychain password will be removed.",
    ));
    // HIG (Alerts): the default button goes on the trailing side and Cancel
    // is never the default. The first button added is the trailing default.
    alert.addButtonWithTitle(&NSString::from_str("Delete"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.runModal() == FIRST_BUTTON
}

/// Show a simple informational / error alert.
pub fn show_error(mtm: MainThreadMarker, title: &str, message: &str) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}

/// Build an `NSRect`/`CGRect` from a size in points.
pub fn rect(width: CGFloat, height: CGFloat) -> CGRect {
    CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height))
}
