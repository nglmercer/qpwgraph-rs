//! Resolve Windows backend icon references to images for graph nodes.
//!
//! The Windows backend supplies one of three reference shapes in
//! `Node::icon_name` (see `pw_graph_backend::windows` icon provider and the
//! `WINDOWS_*_ICON` sentinels in `pw_graph_core`):
//!
//! * a full executable path — the icon is extracted from the binary,
//! * a device icon resource reference (`module path,-index`),
//! * a `windows:...` sentinel — mapped to a stock system icon.
//!
//! The pure parsing helpers below compile on every platform so their tests
//! run anywhere; only the `HICON` extraction itself needs Win32.

#[cfg(any(target_os = "windows", test))]
use pw_graph_core::{WINDOWS_ENDPOINT_CAPTURE_ICON, WINDOWS_ENDPOINT_RENDER_ICON};
use slint::Image;
use std::path::Path;

#[cfg(target_os = "windows")]
use slint::{Rgba8Pixel, SharedPixelBuffer};
#[cfg(target_os = "windows")]
use windows::core::PCWSTR;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
#[cfg(target_os = "windows")]
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Shell::{
    ExtractIconExW, SHGetFileInfoW, SHGetStockIconInfo, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON,
    SHGSI_ICON, SHSTOCKICONID, SHSTOCKICONINFO, SIID_AUDIOFILES, SIID_DEVICEAUDIOPLAYER,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Load a Windows backend icon reference. Anything unresolvable is treated
/// as normal: nodes without usable icon data simply render without an image.
#[cfg(target_os = "windows")]
pub(super) fn load_windows_icon(icon_name: &str) -> Option<Image> {
    let _guard = lock_extraction();
    load_windows_icon_locked(icon_name.trim())
}

// Shell icon retrieval (`SHGetStockIconInfo`, `SHGetFileInfoW`) can fail
// with `E_OUTOFMEMORY` when first called concurrently from several threads
// while the system image list initializes. Extraction is rare and cached by
// the caller, so serialize it process-wide; production calls already arrive
// serially from the UI thread.
#[cfg(target_os = "windows")]
static EXTRACTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "windows")]
fn lock_extraction() -> std::sync::MutexGuard<'static, ()> {
    EXTRACTION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(target_os = "windows")]
fn load_windows_icon_locked(name: &str) -> Option<Image> {
    if name.is_empty() {
        return None;
    }
    if name == WINDOWS_ENDPOINT_RENDER_ICON {
        return stock_icon(SIID_DEVICEAUDIOPLAYER);
    }
    if name == WINDOWS_ENDPOINT_CAPTURE_ICON {
        return stock_icon(SIID_AUDIOFILES);
    }
    if let Some((module, index)) = parse_icon_resource(name) {
        return extract_resource_icon(&module, index);
    }
    if !Path::new(name).is_file() {
        return None;
    }
    if has_icon_module_extension(Path::new(name)) {
        return file_icon(name);
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub(super) fn load_windows_icon(_icon_name: &str) -> Option<Image> {
    None
}

/// Split a device icon resource reference (`module path,-index`) into its
/// parts, expanding any `%VARIABLE%` environment prefixes. A plain file path
/// without a resource index is not a resource reference and yields `None`.
pub(super) fn parse_icon_resource(reference: &str) -> Option<(String, i32)> {
    let reference = reference
        .trim()
        .trim_start_matches('@')
        .trim()
        .trim_matches('"')
        .trim();
    if reference.is_empty() {
        return None;
    }
    let (module, index) = reference.rsplit_once(',')?;
    let index: i32 = index.trim().parse().ok()?;
    let module = expand_windows_env(module.trim().trim_matches('"').trim());
    (!module.is_empty()).then_some((module, index))
}

/// Expand `%NAME%` environment prefixes the way icon resource references use
/// them (`%SystemRoot%\System32\mmres.dll,-300`). Unknown or unterminated
/// variables are left untouched.
pub(super) fn expand_windows_env(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            break;
        };
        let name = &after[..end];
        expanded.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(replacement) => expanded.push_str(&replacement),
            Err(_) => {
                expanded.push('%');
                expanded.push_str(name);
                expanded.push('%');
            }
        }
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    expanded
}

fn has_icon_module_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe" | "dll" | "mui" | "cpl" | "scr" | "ocx" | "ax" | "ico"
            )
        })
}

#[cfg(target_os = "windows")]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Extract the `index`-th icon from a module (`dll`/`exe`/…) resource
/// reference. Negative indices address a resource ID, matching the
/// `path,-id` convention device icon paths use.
#[cfg(target_os = "windows")]
fn extract_resource_icon(module: &str, index: i32) -> Option<Image> {
    let file = wide_null(module);
    let mut large: HICON = unsafe { std::mem::zeroed() };
    let extracted =
        unsafe { ExtractIconExW(PCWSTR(file.as_ptr()), index, Some(&mut large), None, 1) };
    if extracted == 0 || large.is_invalid() {
        return None;
    }
    let image = hicon_to_image(large);
    let _ = unsafe { DestroyIcon(large) };
    image
}

/// Icon the shell associates with a file, falling back to the binary's first
/// resource icon. `SHGetFileInfoW` still answers for binaries without an
/// embedded icon (with the generic executable glyph), which is why it is
/// tried before raw resource extraction.
#[cfg(target_os = "windows")]
fn file_icon(path: &str) -> Option<Image> {
    let wide = wide_null(path);
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let selected = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if selected != 0 && !info.hIcon.is_invalid() {
        let image = hicon_to_image(info.hIcon);
        let _ = unsafe { DestroyIcon(info.hIcon) };
        if image.is_some() {
            return image;
        }
    }
    extract_resource_icon(path, 0)
}

#[cfg(target_os = "windows")]
fn stock_icon(id: SHSTOCKICONID) -> Option<Image> {
    let mut info: SHSTOCKICONINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
    unsafe { SHGetStockIconInfo(id, SHGSI_ICON, &mut info).ok()? };
    if info.hIcon.is_invalid() {
        return None;
    }
    let image = hicon_to_image(info.hIcon);
    let _ = unsafe { DestroyIcon(info.hIcon) };
    image
}

/// Convert an `HICON` to a Slint image via its color and mask bitmaps.
///
/// The color bitmap carries premultiplied-correct RGB but its alpha channel
/// is zero for icons authored without one; those fall back to the monochrome
/// AND mask (set bits are transparent). Monochrome-only icons have no color
/// bitmap and are left to the caller as unresolvable.
#[cfg(target_os = "windows")]
fn hicon_to_image(icon: HICON) -> Option<Image> {
    if icon.is_invalid() {
        return None;
    }
    unsafe {
        let mut icon_info: ICONINFO = std::mem::zeroed();
        GetIconInfo(icon, &mut icon_info).ok()?;

        struct BitmapGuard {
            color: HBITMAP,
            mask: HBITMAP,
        }
        impl Drop for BitmapGuard {
            fn drop(&mut self) {
                unsafe {
                    if !self.color.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.color.0));
                    }
                    if !self.mask.is_invalid() {
                        let _ = DeleteObject(HGDIOBJ(self.mask.0));
                    }
                }
            }
        }
        let _guard = BitmapGuard {
            color: icon_info.hbmColor,
            mask: icon_info.hbmMask,
        };

        if icon_info.hbmColor.is_invalid() {
            return None;
        }
        let mut bitmap: BITMAP = std::mem::zeroed();
        if GetObjectW(
            HGDIOBJ(icon_info.hbmColor.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let width = u32::try_from(bitmap.bmWidth).ok()?;
        let height = u32::try_from(bitmap.bmHeight).ok()?;
        if width == 0 || height == 0 || width > 256 || height > 256 {
            return None;
        }

        let mut descriptor: BITMAPINFO = std::mem::zeroed();
        descriptor.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        descriptor.bmiHeader.biWidth = width as i32;
        // Negative height requests top-down rows, matching Slint's row order.
        descriptor.bmiHeader.biHeight = -(height as i32);
        descriptor.bmiHeader.biPlanes = 1;
        descriptor.bmiHeader.biBitCount = 32;

        let plane = (width as usize) * (height as usize);
        let mut color_bits = vec![0u8; plane * 4];
        // `GetDIBits` rejects a null device context with
        // `ERROR_INVALID_PARAMETER`; the screen DC is only used for palette
        // resolution, which a 32-bit `BI_RGB` read does not need.
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return None;
        }
        let color_lines = GetDIBits(
            screen_dc,
            icon_info.hbmColor,
            0,
            height,
            Some(color_bits.as_mut_ptr() as *mut _),
            &mut descriptor,
            DIB_RGB_COLORS,
        );
        if color_lines == 0 {
            let _ = ReleaseDC(None, screen_dc);
            return None;
        }

        let has_alpha = color_bits.chunks_exact(4).any(|pixel| pixel[3] != 0);
        let mask_bits: Option<Vec<u8>> = if has_alpha || icon_info.hbmMask.is_invalid() {
            None
        } else {
            let mut mask_bits = vec![0u8; plane * 4];
            let read = GetDIBits(
                screen_dc,
                icon_info.hbmMask,
                0,
                height,
                Some(mask_bits.as_mut_ptr() as *mut _),
                &mut descriptor,
                DIB_RGB_COLORS,
            );
            (read != 0).then_some(mask_bits)
        };
        let _ = ReleaseDC(None, screen_dc);

        let mut pixels = Vec::with_capacity(plane);
        for (index, color) in color_bits.chunks_exact(4).enumerate() {
            let alpha = if has_alpha {
                color[3]
            } else {
                // The AND mask marks transparent pixels with set bits; a
                // missing mask means the color bitmap is fully opaque.
                match mask_bits.as_ref() {
                    Some(mask) => {
                        let mask_pixel = &mask[index * 4..index * 4 + 3];
                        let luminance = u16::from(mask_pixel[0])
                            + u16::from(mask_pixel[1])
                            + u16::from(mask_pixel[2]);
                        if luminance > 384 {
                            0
                        } else {
                            255
                        }
                    }
                    None => 255,
                }
            };
            pixels.push(Rgba8Pixel {
                r: color[2],
                g: color[1],
                b: color[0],
                a: alpha,
            });
        }

        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
        buffer.make_mut_slice().copy_from_slice(&pixels);
        Some(Image::from_rgba8(buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_windows_env, has_icon_module_extension, parse_icon_resource};
    use std::path::Path;

    #[test]
    fn parses_a_device_icon_resource_reference() {
        std::env::set_var("QPWGRAPH_RS_ICON_TEST", r"C:\Icons");
        let parsed = parse_icon_resource(r"%QPWGRAPH_RS_ICON_TEST%\audio.dll,-300");
        std::env::remove_var("QPWGRAPH_RS_ICON_TEST");
        assert_eq!(parsed, Some((r"C:\Icons\audio.dll".to_owned(), -300)));
    }

    #[test]
    fn parses_quoted_and_at_prefixed_references() {
        assert_eq!(
            parse_icon_resource(r#"@"%SystemRoot%\System32\mmres.dll", 2"#),
            {
                let expanded = expand_windows_env(r"%SystemRoot%\System32\mmres.dll");
                Some((expanded, 2))
            }
        );
        assert_eq!(
            parse_icon_resource(r#""C:\Windows\System32\imageres.dll",0"#),
            Some((r"C:\Windows\System32\imageres.dll".to_owned(), 0))
        );
    }

    #[test]
    fn plain_paths_are_not_resource_references() {
        assert_eq!(parse_icon_resource(r"C:\Program Files\App\app.exe"), None);
        assert_eq!(parse_icon_resource("firefox"), None);
        assert_eq!(parse_icon_resource(""), None);
        assert_eq!(parse_icon_resource(r"C:\icons\app.dll,not-an-index"), None);
        assert_eq!(parse_icon_resource(r",3"), None);
    }

    #[test]
    fn environment_expansion_keeps_unknown_or_unterminated_variables() {
        std::env::set_var("QPWGRAPH_RS_ICON_KEEP", "kept");
        let expanded = expand_windows_env(r"%QPWGRAPH_RS_ICON_KEEP%\x.dll");
        std::env::remove_var("QPWGRAPH_RS_ICON_KEEP");
        assert_eq!(expanded, r"kept\x.dll");
        assert_eq!(
            expand_windows_env(r"%QPWGRAPH_RS_ICON_MISSING%\x.dll"),
            r"%QPWGRAPH_RS_ICON_MISSING%\x.dll"
        );
        assert_eq!(
            expand_windows_env(r"C:\icons\%unterminated"),
            r"C:\icons\%unterminated"
        );
    }

    #[test]
    fn icon_module_extensions_cover_binaries_and_icon_files() {
        for name in ["app.exe", "audio.dll", "res.mui", "setup.cpl", "icon.ico"] {
            assert!(
                has_icon_module_extension(Path::new(name)),
                "{name} should be treated as an extractable module"
            );
        }
        for name in ["icon.png", "icon.svg", "photo.jpg", "no-extension"] {
            assert!(
                !has_icon_module_extension(Path::new(name)),
                "{name} should be left to the generic image loader"
            );
        }
    }

    #[test]
    fn windows_sentinels_stay_in_their_own_namespace() {
        assert_ne!(
            super::WINDOWS_ENDPOINT_RENDER_ICON,
            super::WINDOWS_ENDPOINT_CAPTURE_ICON
        );
        for sentinel in [
            super::WINDOWS_ENDPOINT_RENDER_ICON,
            super::WINDOWS_ENDPOINT_CAPTURE_ICON,
        ] {
            assert!(sentinel.starts_with("windows:"));
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn extracts_an_icon_from_the_running_test_binary() {
        let _guard = super::lock_extraction();
        let binary = std::env::current_exe().expect("test binary must have a path");
        let image = super::file_icon(&binary.to_string_lossy())
            .expect("the shell must provide at least a generic icon for an executable");
        assert!(image.size().width > 0 && image.size().height > 0);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn stock_icons_resolve_for_every_sentinel() {
        let _guard = super::lock_extraction();
        use super::{stock_icon, SIID_AUDIOFILES, SIID_DEVICEAUDIOPLAYER};
        for id in [SIID_DEVICEAUDIOPLAYER, SIID_AUDIOFILES] {
            let image = stock_icon(id).expect("stock icon must resolve");
            assert!(image.size().width > 0 && image.size().height > 0);
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn end_to_end_windows_references_resolve() {
        // Sentinels plus the running binary cover every branch the backend
        // can emit: endpoint fallbacks and session executables.
        assert!(super::load_windows_icon(super::WINDOWS_ENDPOINT_RENDER_ICON).is_some());
        assert!(super::load_windows_icon(super::WINDOWS_ENDPOINT_CAPTURE_ICON).is_some());
        let binary = std::env::current_exe().expect("test binary must have a path");
        assert!(super::load_windows_icon(&binary.to_string_lossy()).is_some());
        assert!(super::load_windows_icon("not a real icon reference \u{1}").is_none());
    }
}
