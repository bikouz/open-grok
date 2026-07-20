//! HEIC/HEIF → JPEG transcoding.
//!
//! The `image` crate cannot decode HEIC (HEVC-in-HEIF) and no production
//! pure-Rust HEVC decoder exists, so Apple-camera files are transcoded to
//! JPEG at ingestion and every downstream consumer (vision upload, session
//! persistence, terminal preview) only ever sees JPEG.
//!
//! Reliability contract:
//! - **macOS (the shipped platform)**: conversion is fully in-process via
//!   the OS ImageIO framework — the same decoder `sips` wraps — linked at
//!   build time. No subprocess, no PATH lookup, no installable dependency.
//! - **Other platforms**: best-effort shell-out to `heif-convert`
//!   (libheif-examples) or ImageMagick when present. No converter available
//!   is a hard, explained error — never a silent passthrough of bytes the
//!   API/preview cannot use.

/// JPEG encode quality for transcoded HEICs, matching the mid-quality rung
/// of the read-tool compression ladder (the compress path may re-encode
/// again anyway).
const JPEG_QUALITY: f64 = 0.85;

/// Mime types this module transcodes (as sniffed by `infer`).
pub fn is_heic_mime(mime: &str) -> bool {
    matches!(mime, "image/heic" | "image/heif")
}

/// ISO-BMFF `ftyp` brand sniff for HEIC/HEIF stills.
///
/// Mirrors the brand set `infer` maps to `image/heic`/`image/heif`
/// (major brand `heic`/`heix`/`mif1`/`msf1`, plus HEVC-sequence brands).
/// AVIF (`avif`/`avis`) is deliberately excluded — different codec,
/// different converters.
pub fn is_heic_bytes(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    const BRANDS: &[&[u8; 4]] = &[
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    ];
    let brand: &[u8] = &bytes[8..12];
    BRANDS.iter().any(|b| *b == brand)
}

/// Transcode HEIC/HEIF bytes to JPEG, blocking. Call from a background
/// thread (or via [`convert_heic_to_jpeg`]) on latency-sensitive paths.
pub fn convert_heic_to_jpeg_blocking(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        imageio::heic_to_jpeg(bytes, JPEG_QUALITY)
    }
    #[cfg(not(target_os = "macos"))]
    {
        external::heic_to_jpeg(bytes)
    }
}

/// Async wrapper over [`convert_heic_to_jpeg_blocking`].
pub async fn convert_heic_to_jpeg(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || convert_heic_to_jpeg_blocking(&bytes))
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "HEIC conversion task panicked");
            "HEIC conversion failed; see logs.".to_owned()
        })?
}

/// In-process conversion through the macOS ImageIO framework.
///
/// Raw C FFI against CoreFoundation + ImageIO — both ship with the OS and
/// are linked at build time, so this path has no runtime dependency at all.
/// Every CF object is released via [`Released`] so early returns cannot leak.
#[cfg(target_os = "macos")]
mod imageio {
    use std::ffi::{c_char, c_double, c_long, c_void};

    type CFTypeRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFMutableDataRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CGImageSourceRef = *const c_void;
    type CGImageDestinationRef = *const c_void;
    type CFIndex = c_long;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// `kCFNumberDoubleType`
    const K_CF_NUMBER_DOUBLE_TYPE: CFIndex = 13;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFRelease(cf: CFTypeRef);
        fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: CFIndex) -> CFDataRef;
        fn CFDataCreateMutable(allocator: *const c_void, capacity: CFIndex) -> CFMutableDataRef;
        fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
        fn CFDataGetLength(data: CFDataRef) -> CFIndex;
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFNumberCreate(
            allocator: *const c_void,
            the_type: CFIndex,
            value_ptr: *const c_void,
        ) -> CFNumberRef;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            num_values: CFIndex,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
    }

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        static kCGImageDestinationLossyCompressionQuality: CFStringRef;
        fn CGImageSourceCreateWithData(
            data: CFDataRef,
            options: CFDictionaryRef,
        ) -> CGImageSourceRef;
        fn CGImageSourceGetCount(source: CGImageSourceRef) -> usize;
        fn CGImageSourceGetPrimaryImageIndex(source: CGImageSourceRef) -> usize;
        fn CGImageDestinationCreateWithData(
            data: CFMutableDataRef,
            type_id: CFStringRef,
            count: usize,
            options: CFDictionaryRef,
        ) -> CGImageDestinationRef;
        fn CGImageDestinationAddImageFromSource(
            dest: CGImageDestinationRef,
            source: CGImageSourceRef,
            index: usize,
            properties: CFDictionaryRef,
        );
        fn CGImageDestinationFinalize(dest: CGImageDestinationRef) -> bool;
    }

    /// Owned CF object, `CFRelease`d on drop. `new` maps NULL to `Err` so
    /// creation failures surface as errors instead of segfaults.
    struct Released(CFTypeRef);

    impl Released {
        fn new(cf: CFTypeRef, what: &'static str) -> Result<Self, String> {
            if cf.is_null() {
                Err(format!("ImageIO: failed to create {what}"))
            } else {
                Ok(Self(cf))
            }
        }
    }

    impl Drop for Released {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    pub(super) fn heic_to_jpeg(bytes: &[u8], quality: f64) -> Result<Vec<u8>, String> {
        unsafe {
            let data = Released::new(
                CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex),
                "input data",
            )?;
            let source = Released::new(
                CGImageSourceCreateWithData(data.0, std::ptr::null()),
                "image source (undecodable HEIC?)",
            )?;
            if CGImageSourceGetCount(source.0) == 0 {
                return Err("ImageIO: HEIC container holds no images".to_owned());
            }
            // Multi-image HEICs (bursts, live photos) store the display
            // image at the primary index, not necessarily 0.
            let index = CGImageSourceGetPrimaryImageIndex(source.0);

            let output = Released::new(CFDataCreateMutable(std::ptr::null(), 0), "output buffer")?;
            let jpeg_type = Released::new(
                CFStringCreateWithCString(
                    std::ptr::null(),
                    c"public.jpeg".as_ptr(),
                    K_CF_STRING_ENCODING_UTF8,
                ),
                "jpeg type identifier",
            )?;
            let dest = Released::new(
                CGImageDestinationCreateWithData(output.0, jpeg_type.0, 1, std::ptr::null()),
                "jpeg destination",
            )?;

            let quality_value: c_double = quality;
            let quality_number = Released::new(
                CFNumberCreate(
                    std::ptr::null(),
                    K_CF_NUMBER_DOUBLE_TYPE,
                    (&raw const quality_value).cast(),
                ),
                "quality number",
            )?;
            let keys = [kCGImageDestinationLossyCompressionQuality];
            let values = [quality_number.0];
            let options = Released::new(
                CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &raw const kCFTypeDictionaryKeyCallBacks,
                    &raw const kCFTypeDictionaryValueCallBacks,
                ),
                "destination options",
            )?;

            CGImageDestinationAddImageFromSource(dest.0, source.0, index, options.0);
            if !CGImageDestinationFinalize(dest.0) {
                return Err("ImageIO: JPEG encode failed".to_owned());
            }

            let len = CFDataGetLength(output.0);
            let ptr = CFDataGetBytePtr(output.0);
            if len <= 0 || ptr.is_null() {
                return Err("ImageIO: JPEG encode produced no bytes".to_owned());
            }
            let jpeg = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            if !jpeg.starts_with(&[0xFF, 0xD8]) {
                return Err("ImageIO: JPEG encode produced non-JPEG bytes".to_owned());
            }
            Ok(jpeg)
        }
    }
}

/// Best-effort shell-out for platforms without an OS-native decoder.
#[cfg(not(target_os = "macos"))]
mod external {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// One converter invocation: program plus the args that turn
    /// `input` into a JPEG at `output`.
    fn converter_commands(
        input: &Path,
        output: &Path,
    ) -> Vec<(&'static str, Vec<std::ffi::OsString>)> {
        vec![
            (
                "heif-convert",
                vec!["-q".into(), "85".into(), input.into(), output.into()],
            ),
            ("magick", vec![input.into(), output.into()]),
        ]
    }

    /// Temp-file pair for one conversion, removed on drop.
    struct TempConversion {
        input: PathBuf,
        output: PathBuf,
    }

    impl Drop for TempConversion {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.input);
            let _ = std::fs::remove_file(&self.output);
        }
    }

    pub(super) fn heic_to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
        let stem = std::env::temp_dir().join(format!("opengrok-heic-{}", uuid::Uuid::new_v4()));
        let temp = TempConversion {
            input: stem.with_extension("heic"),
            output: stem.with_extension("jpg"),
        };
        std::fs::write(&temp.input, bytes)
            .map_err(|e| format!("failed to stage HEIC bytes for conversion: {e}"))?;

        let mut attempts: Vec<String> = Vec::new();
        for (program, args) in converter_commands(&temp.input, &temp.output) {
            let run = Command::new(program).args(&args).output();
            match run {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    attempts.push(format!("{program}: not installed"));
                    continue;
                }
                Err(e) => {
                    attempts.push(format!("{program}: {e}"));
                    continue;
                }
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    attempts.push(format!(
                        "{program}: exit {} ({})",
                        output.status.code().unwrap_or(-1),
                        stderr.trim().lines().next().unwrap_or("no error output"),
                    ));
                    continue;
                }
                Ok(_) => {}
            }
            match std::fs::read(&temp.output) {
                // SOI magic guards against converters that "succeed" while
                // writing an empty or non-JPEG file.
                Ok(jpeg) if jpeg.starts_with(&[0xFF, 0xD8]) => return Ok(jpeg),
                Ok(_) => attempts.push(format!("{program}: produced non-JPEG output")),
                Err(e) => attempts.push(format!("{program}: no output file ({e})")),
            }
        }
        Err(format!(
            "no HEIC converter succeeded [{}]. Install libheif-examples (heif-convert) \
             or ImageMagick, or convert the file to JPEG/PNG manually.",
            attempts.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0, 0, 0, 24];
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(brand);
        b.extend_from_slice(&[0; 12]);
        b
    }

    #[test]
    fn sniffs_heic_brands_and_rejects_others() {
        for brand in [b"heic", b"heix", b"mif1", b"msf1", b"hevc"] {
            assert!(is_heic_bytes(&ftyp(brand)), "{brand:?}");
        }
        assert!(!is_heic_bytes(&ftyp(b"avif")), "AVIF is out of scope");
        assert!(!is_heic_bytes(&ftyp(b"isom")));
        assert!(!is_heic_bytes(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_heic_bytes(b""));
    }

    #[test]
    fn mime_gate_matches_infer_output() {
        assert!(is_heic_mime("image/heic"));
        assert!(is_heic_mime("image/heif"));
        assert!(!is_heic_mime("image/avif"));
        assert!(!is_heic_mime("image/jpeg"));
    }

    #[test]
    fn conversion_of_garbage_fails_with_explanation() {
        let error = convert_heic_to_jpeg_blocking(&ftyp(b"heic"))
            .expect_err("a bare ftyp header is not a decodable image");
        assert!(!error.is_empty());
    }

    /// Real round-trip through the in-process ImageIO path. macOS-only:
    /// uses the always-present `sips` to author a genuine HEIC fixture
    /// (test-only; runtime conversion never shells out on macOS).
    #[cfg(target_os = "macos")]
    #[test]
    fn converts_real_heic_to_decodable_jpeg() {
        let stem =
            std::env::temp_dir().join(format!("opengrok-heic-test-{}", uuid::Uuid::new_v4()));
        let png_path = stem.with_extension("png");
        let heic_path = stem.with_extension("heic");

        let img = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8, y as u8, 128]));
        img.save(&png_path).unwrap();
        let status = std::process::Command::new("sips")
            .args(["-s", "format", "heic"])
            .arg(&png_path)
            .arg("--out")
            .arg(&heic_path)
            .status()
            .expect("sips is part of macOS");
        assert!(status.success(), "sips could not author the HEIC fixture");
        let heic_bytes = std::fs::read(&heic_path).unwrap();
        let _ = std::fs::remove_file(&png_path);
        let _ = std::fs::remove_file(&heic_path);
        assert!(is_heic_bytes(&heic_bytes), "fixture must sniff as HEIC");

        let jpeg = convert_heic_to_jpeg_blocking(&heic_bytes).expect("conversion");
        let decoded = image::load_from_memory(&jpeg).expect("output must decode");
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
    }
}
