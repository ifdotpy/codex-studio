//! Rasterize one PDF page from an immutable file snapshot. No document actions run.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{io::Cursor, sync::Arc};

pub async fn page(bytes: Arc<Vec<u8>>, page: u64) -> Result<Value, String> {
    // The permit lives in the blocking task even when the caller disconnects.
    static LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let permit = LIMIT.acquire().await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        render(&bytes, page)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn render(_: &[u8], _: u64) -> Result<Value, String> {
    Err("PDF pages are not available on this platform.".into())
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn render(bytes: &[u8], index: u64) -> Result<Value, String> {
    use objc2_core_foundation::{CFData, CGPoint, CGRect, CGSize};
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGImageAlphaInfo,
        CGImageByteOrderInfo, CGPDFBox, CGPDFDocument, CGPDFPage,
    };
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return Err("This PDF exceeds the 20 MiB preview limit or is empty.".into());
    }
    let data = CFData::from_bytes(bytes);
    let provider = CGDataProvider::with_cf_data(Some(&data)).ok_or("Cannot read this PDF.")?;
    let document = CGPDFDocument::with_provider(Some(&provider)).ok_or("Invalid PDF file.")?;
    if !CGPDFDocument::is_unlocked(Some(&document)) {
        return Err("Open this PDF in a PDF application to enter its password.".into());
    }
    let pages = CGPDFDocument::number_of_pages(Some(&document));
    if pages == 0 || pages > 10_000 || index >= pages as u64 {
        return Err("Invalid PDF page or document page limit exceeded.".into());
    }
    let page = CGPDFDocument::page(Some(&document), index as usize + 1)
        .ok_or("Cannot read this PDF page.")?;
    let bounds = CGPDFPage::box_rect(Some(&page), CGPDFBox::CropBox);
    let mut size = bounds.size;
    if [bounds.origin.x, bounds.origin.y, size.width, size.height]
        .iter()
        .any(|v| !v.is_finite())
        || bounds.origin.x.abs() > 1_000_000.0
        || bounds.origin.y.abs() > 1_000_000.0
        || size.width < 0.01
        || size.height < 0.01
        || size.width > 1_000_000.0
        || size.height > 1_000_000.0
    {
        return Err("Invalid PDF page dimensions.".into());
    }
    if CGPDFPage::rotation_angle(Some(&page)).rem_euclid(180) == 90 {
        std::mem::swap(&mut size.width, &mut size.height);
    }
    let scale = (1440.0 / size.width).min(1920.0 / size.height);
    let width = (size.width * scale).round().max(1.0) as u32;
    let height = (size.height * scale).round().max(1.0) as u32;
    let mut pixels = vec![0_u8; width as usize * height as usize * 4];
    let space = CGColorSpace::new_device_rgb().ok_or("Cannot create the PDF color space.")?;
    // RGBA bytes, big-endian components, premultiplied alpha. The opaque white
    // background makes the result ordinary RGBA for the PNG encoder.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width as usize,
            height as usize,
            8,
            width as usize * 4,
            Some(&space),
            CGImageByteOrderInfo::Order32Big.0 | CGImageAlphaInfo::PremultipliedLast.0,
        )
    }
    .ok_or("Cannot create the PDF page image.")?;
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: width as f64,
            height: height as f64,
        },
    };
    CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(Some(&context), rect);
    // CoreGraphics' page transform only scales down. Scale the context first
    // to preserve small documents at the requested raster resolution.
    CGContext::scale_ctm(
        Some(&context),
        width as f64 / size.width,
        height as f64 / size.height,
    );
    let page_rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size,
    };
    CGContext::concat_ctm(
        Some(&context),
        CGPDFPage::drawing_transform(Some(&page), CGPDFBox::CropBox, page_rect, 0, true),
    );
    CGContext::draw_pdf_page(Some(&context), Some(&page));
    CGContext::flush(Some(&context));
    drop(context); // Release the native borrow before reading the pixel buffer.
    let image =
        image::RgbaImage::from_raw(width, height, pixels).ok_or("Invalid PDF page image.")?;
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    if png.get_ref().len() > 16 * 1024 * 1024 {
        return Err("The PDF page image is too large.".into());
    }
    Ok(
        json!({"url":format!("data:image/png;base64,{}", STANDARD.encode(png.into_inner())),
        "width":width,"height":height,"page":index,"pages":pages}),
    )
}

#[cfg(all(test, any(target_os = "macos", target_os = "ios")))]
mod tests {
    use super::*;
    // A real two-page PDF with top/bottom colors and a rotated second page.
    #[tokio::test]
    async fn pages_have_real_pixels_crop_rotation_and_bounds() {
        let bytes = Arc::new(include_bytes!("../tests/fixtures/pages.pdf").to_vec());
        let first = page(bytes.clone(), 0).await.unwrap();
        assert_eq!(first["pages"], 2);
        let decode = |view: &Value| {
            image::load_from_memory(
                &STANDARD
                    .decode(view["url"].as_str().unwrap().split_once(',').unwrap().1)
                    .unwrap(),
            )
            .unwrap()
            .to_rgb8()
        };
        let image = decode(&first);
        assert_eq!(image.dimensions(), (1440, 1440));
        assert_eq!(image.get_pixel(720, 360).0, [255, 0, 0]);
        assert_eq!(image.get_pixel(720, 1080).0, [0, 0, 255]);
        let second = page(bytes.clone(), 1).await.unwrap();
        let image = decode(&second);
        assert_eq!(image.dimensions(), (960, 1920));
        assert_eq!(image.get_pixel(480, 960).0, [255, 0, 0]);
        assert!(page(bytes, 2).await.is_err());
        assert!(page(Arc::new(b"not a PDF".to_vec()), 0).await.is_err());
    }
}
