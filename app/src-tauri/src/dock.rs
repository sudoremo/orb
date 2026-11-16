//! The badges on the Dock icon.
//!
//! macOS gives an app one badge label. Without `dock_badges` that is all Orb
//! uses; with it, the icon is drawn here with a coloured badge per count, one
//! corner each — top right, top left, bottom right, bottom left, in the
//! configured order — as the Dock tile's content view.

use orb_core::ffi::DockBadgeCount;
use tauri::AppHandle;

/// What the Dock icon shows.
pub enum Badges {
    /// The system's own badge, with this count; none at zero.
    Native(u32),
    /// Drawn badges, in corner order; the plain icon when every count is zero.
    Drawn(Vec<DockBadgeCount>),
}

/// Put `badges` on the Dock icon. AppKit is only touched on the main thread.
pub fn show(app: &AppHandle, badges: Badges) {
    if let Err(error) = app.run_on_main_thread(move || apply(&badges)) {
        orb_core::logger::warn(&format!("dock: the badges were not drawn: {error}"));
    }
}

/// A count as the badge says it: past 999 it stops counting.
fn label(count: u32) -> String {
    if count > 999 {
        "999+".to_string()
    } else {
        count.to_string()
    }
}

/// `#rrggbb` as red, green and blue between 0 and 1.
fn rgb(color: &str) -> Option<(f64, f64, f64)> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |at: usize| {
        u8::from_str_radix(&hex[at..at + 2], 16)
            .ok()
            .map(|v| f64::from(v) / 255.0)
    };
    Some((channel(0)?, channel(2)?, channel(4)?))
}

#[cfg(not(target_os = "macos"))]
fn apply(_: &Badges) {}

#[cfg(target_os = "macos")]
fn apply(badges: &Badges) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let tile = app.dockTile();
    match badges {
        Badges::Native(count) => {
            tile.setContentView(None);
            let text = (*count > 0).then(|| NSString::from_str(&label(*count)));
            tile.setBadgeLabel(text.as_deref());
        }
        Badges::Drawn(counts) if counts.iter().all(|badge| badge.count == 0) => {
            tile.setContentView(None);
            tile.setBadgeLabel(None);
        }
        Badges::Drawn(counts) => {
            tile.setBadgeLabel(None);
            let view = drawn_icon(&app, tile.size(), counts, mtm);
            tile.setContentView(Some(&view));
        }
    }
    tile.display();
}

/// The application icon with a badge per non-zero count, as a view the
/// Dock tile can show.
#[cfg(target_os = "macos")]
fn drawn_icon(
    app: &objc2_app_kit::NSApplication,
    size: objc2_foundation::NSSize,
    counts: &[DockBadgeCount],
    mtm: objc2::MainThreadMarker,
) -> objc2::rc::Retained<objc2_app_kit::NSView> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSImageView;
    use objc2_foundation::NSSize;

    let size = if size.width > 0.0 && size.height > 0.0 {
        size
    } else {
        NSSize::new(128.0, 128.0)
    };
    let image = badged_image(app.applicationIconImage(), size, counts);
    Retained::into_super(Retained::into_super(NSImageView::imageViewWithImage(
        &image, mtm,
    )))
}

/// `icon` with a badge per non-zero count. The image draws itself when
/// shown, at the screen's resolution.
#[cfg(target_os = "macos")]
fn badged_image(
    icon: Option<objc2::rc::Retained<objc2_app_kit::NSImage>>,
    size: objc2_foundation::NSSize,
    counts: &[DockBadgeCount],
) -> objc2::rc::Retained<objc2_app_kit::NSImage> {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSRect;

    let counts = counts.to_vec();
    let draw = RcBlock::new(move |bounds: NSRect| {
        if let Some(icon) = &icon {
            icon.drawInRect(bounds);
        }
        for (corner, badge) in counts.iter().enumerate() {
            if badge.count > 0 {
                draw_badge(bounds.size, corner, badge);
            }
        }
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw)
}

/// One badge in its corner: 0 top right, 1 top left, 2 bottom right,
/// 3 bottom left.
#[cfg(target_os = "macos")]
fn draw_badge(size: objc2_foundation::NSSize, corner: usize, badge: &DockBadgeCount) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_app_kit::{
        NSAttributedStringNSStringDrawing, NSBezierPath, NSColor, NSFont, NSFontAttributeName,
        NSForegroundColorAttributeName,
    };
    use objc2_foundation::{NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString};

    // About the native badge's size against the tile.
    let height = size.height * 0.34;
    let font = NSFont::boldSystemFontOfSize(height * 0.62);
    let white = NSColor::whiteColor();
    // SAFETY: AppKit's attribute keys are immutable constants.
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values: [&AnyObject; 2] = [font.as_ref(), white.as_ref()];
    let attributes = NSDictionary::from_slices(&keys, &values);
    // SAFETY: the attributes are a font and a colour under the keys AppKit
    // defines for them.
    let text = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(&label(badge.count)),
            Some(&attributes),
        )
    };
    let text_size = text.size();
    let width = height.max(text_size.width + height * 0.6);
    let right = corner.is_multiple_of(2);
    let top = corner < 2;
    let origin = NSPoint::new(
        if right { size.width - width } else { 0.0 },
        if top { size.height - height } else { 0.0 },
    );

    let fill: Retained<NSColor> = match badge.color.as_deref().and_then(rgb) {
        Some((r, g, b)) => NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0),
        None => NSColor::systemRedColor(),
    };
    fill.setFill();
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
        NSRect::new(origin, NSSize::new(width, height)),
        height / 2.0,
        height / 2.0,
    )
    .fill();
    text.drawAtPoint(NSPoint::new(
        origin.x + (width - text_size.width) / 2.0,
        origin.y + (height - text_size.height) / 2.0,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_count_is_cut_short() {
        assert_eq!(label(7), "7");
        assert_eq!(label(999), "999");
        assert_eq!(label(1000), "999+");
    }

    /// Draws four badges on a grey square into `/tmp/orb-dock.png`, to look at.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "writes /tmp/orb-dock.png"]
    fn badges_drawn_to_a_file() {
        use objc2::AnyThread;
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSImage};
        use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize};

        let size = NSSize::new(256.0, 256.0);
        let square = NSImage::initWithSize(NSImage::alloc(), size);
        #[allow(deprecated)]
        {
            square.lockFocus();
            NSColor::grayColor().setFill();
            objc2_app_kit::NSRectFill(NSRect::new(NSPoint::new(0.0, 0.0), size));
            square.unlockFocus();
        }
        let badge = |count, color: Option<&str>| DockBadgeCount {
            count,
            color: color.map(str::to_string),
        };
        let image = badged_image(
            Some(square),
            size,
            &[
                badge(3, None),
                badge(42, Some("#0a84ff")),
                badge(1234, Some("#34c759")),
                badge(7, Some("#ff9f0a")),
            ],
        );
        let tiff = image.TIFFRepresentation().expect("tiff");
        let rep = NSBitmapImageRep::imageRepWithData(&tiff).expect("bitmap");
        // SAFETY: an empty property dictionary.
        let png = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        }
        .expect("png");
        std::fs::write("/tmp/orb-dock.png", png.to_vec()).expect("write");
    }

    #[test]
    fn a_colour_is_read_from_its_hex_form() {
        assert_eq!(rgb("#ff0000"), Some((1.0, 0.0, 0.0)));
        assert_eq!(rgb("ff0000"), None);
        assert_eq!(rgb("#ff00"), None);
    }
}
