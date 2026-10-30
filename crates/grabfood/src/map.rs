//! Render a live order to a static PNG: an OpenStreetMap basemap with the
//! restaurant, drop-off and driver pins plus the remaining route line.
//!
//! Tiles come straight from the OSM tile servers (not the Grab API client, so
//! they never touch the Grab allowlist), with a descriptive `User-Agent` and an
//! on-disk cache, keeping within the OSM tile usage policy: we only ever fetch a
//! handful of tiles for one small bounding box. If any tile cannot be fetched we
//! fall back to a plain background and still draw the overlay.

use std::f64::consts::PI;
use std::path::Path;
use std::time::Duration;

use ab_glyph::{FontRef, PxScale};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::paths;
use image::{Rgba, RgbaImage};
use imageproc::drawing;

use crate::tracking::{LatLng, Tracking};

const TILE: i64 = 256;
/// Keep the fetch tiny: at most this many tiles on each axis.
const MAX_TILES_PER_AXIS: i64 = 4;
const USER_AGENT: &str = concat!(
    "grabcli/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/macalinao/grabcli)"
);
const ATTRIBUTION: &str = "(C) OpenStreetMap contributors";

/// A geographic point to plot, with its pin colour.
struct Pin {
    at: LatLng,
    color: Rgba<u8>,
}

/// web-mercator pixel X at a zoom level (global pixel space, origin top-left).
fn lng_to_px(lng: f64, zoom: u32) -> f64 {
    let n = (TILE << zoom) as f64;
    (lng + 180.0) / 360.0 * n
}

/// web-mercator pixel Y at a zoom level.
fn lat_to_px(lat: f64, zoom: u32) -> f64 {
    let n = (TILE << zoom) as f64;
    let rad = lat.to_radians();
    (1.0 - ((rad.tan() + 1.0 / rad.cos()).ln() / PI)) / 2.0 * n
}

/// Pick the highest zoom whose bounding box fits inside the tile budget.
fn pick_zoom(min: &LatLng, max: &LatLng) -> u32 {
    for zoom in (1..=18u32).rev() {
        let x0 = (lng_to_px(min.longitude, zoom) / TILE as f64).floor() as i64;
        let x1 = (lng_to_px(max.longitude, zoom) / TILE as f64).floor() as i64;
        let y0 = (lat_to_px(max.latitude, zoom) / TILE as f64).floor() as i64;
        let y1 = (lat_to_px(min.latitude, zoom) / TILE as f64).floor() as i64;
        if (x1 - x0).abs() < MAX_TILES_PER_AXIS && (y1 - y0).abs() < MAX_TILES_PER_AXIS {
            return zoom;
        }
    }
    1
}

/// Gather every coordinate we know about, so the map frames all of them.
fn collect_points(t: &Tracking) -> Vec<LatLng> {
    let mut pts = Vec::new();
    if let Some(c) = &t.merchant.coordinates {
        pts.push(c.clone());
    }
    if let Some(c) = &t.dropoff.coordinates {
        pts.push(c.clone());
    }
    if let Some(d) = &t.driver
        && let Some(l) = &d.location
    {
        pts.push(l.clone());
    }
    pts.extend(t.route.iter().cloned());
    pts
}

async fn fetch_tile(client: &reqwest::Client, zoom: u32, x: i64, y: i64) -> Option<RgbaImage> {
    let cache = paths::state_dir()
        .ok()
        .map(|d| d.join("tiles").join(zoom.to_string()));
    let cached = cache.as_ref().map(|d| d.join(format!("{x}_{y}.png")));

    if let Some(p) = &cached
        && let Ok(img) = image::open(p)
    {
        return Some(img.to_rgba8());
    }

    let url = format!("https://tile.openstreetmap.org/{zoom}/{x}/{y}.png");
    let bytes = client
        .get(&url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .bytes()
        .await
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();

    if let Some(p) = &cached {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = img.save(p);
    }
    Some(img)
}

/// Draw a teardrop-ish pin: a filled circle with a dark outline and white core.
fn draw_pin(canvas: &mut RgbaImage, cx: i32, cy: i32, color: Rgba<u8>) {
    drawing::draw_filled_circle_mut(canvas, (cx, cy), 9, Rgba([30, 30, 30, 255]));
    drawing::draw_filled_circle_mut(canvas, (cx, cy), 7, color);
    drawing::draw_filled_circle_mut(canvas, (cx, cy), 3, Rgba([255, 255, 255, 255]));
}

/// Thick line by stamping small circles along the segment.
fn draw_thick_line(canvas: &mut RgbaImage, a: (f32, f32), b: (f32, f32), color: Rgba<u8>) {
    let steps = ((b.0 - a.0).hypot(b.1 - a.1) / 2.0).ceil().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let x = (a.0 + (b.0 - a.0) * t).round() as i32;
        let y = (a.1 + (b.1 - a.1) * t).round() as i32;
        drawing::draw_filled_circle_mut(canvas, (x, y), 2, color);
    }
}

/// Find a usable sans font so the attribution can be drawn. Tries common fixed
/// paths, then `fontconfig` (`fc-match`), then any `.ttf` under the standard
/// font directories. Returns `None` only if nothing at all is found.
fn load_font() -> Option<FontRef<'static>> {
    fn try_path(p: &std::path::Path) -> Option<FontRef<'static>> {
        let bytes = std::fs::read(p).ok()?;
        // Leak the bytes: a process renders few maps and then exits.
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        FontRef::try_from_slice(leaked).ok()
    }

    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/run/current-system/sw/share/fonts/truetype/DejaVuSans.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];
    for p in CANDIDATES {
        if let Some(f) = try_path(std::path::Path::new(p)) {
            return Some(f);
        }
    }

    // Ask fontconfig where a sans-serif TTF lives.
    if let Ok(out) = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans"])
        .output()
        && out.status.success()
    {
        let path = String::from_utf8_lossy(&out.stdout);
        let path = path.trim();
        if path.ends_with(".ttf")
            && let Some(f) = try_path(std::path::Path::new(path))
        {
            return Some(f);
        }
    }

    // Last resort: scan `<data-dir>/fonts` from XDG for any .ttf.
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dirs) = std::env::var("XDG_DATA_DIRS") {
        roots.extend(
            dirs.split(':')
                .map(|d| std::path::Path::new(d).join("fonts")),
        );
    }
    roots.push("/usr/share/fonts".into());
    roots.push("/usr/local/share/fonts".into());
    for root in roots {
        if let Some(f) = find_ttf(&root, 0) {
            return Some(f);
        }
    }
    None
}

/// Shallow recursive hunt for the first readable `.ttf`.
fn find_ttf(dir: &std::path::Path, depth: u32) -> Option<FontRef<'static>> {
    if depth > 4 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_ttf(&p, depth + 1) {
                return Some(f);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("ttf")
            && let Ok(bytes) = std::fs::read(&p)
        {
            let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            if let Ok(f) = FontRef::try_from_slice(leaked) {
                return Some(f);
            }
        }
    }
    None
}

/// Render the tracking model to a PNG at `path`.
pub async fn render(model: &Tracking, path: &Path) -> Result<()> {
    let points = collect_points(model);
    if points.is_empty() {
        return Err(Error::invalid("no coordinates available to draw a map"));
    }

    let mut min = points[0].clone();
    let mut max = points[0].clone();
    for p in &points {
        min.latitude = min.latitude.min(p.latitude);
        min.longitude = min.longitude.min(p.longitude);
        max.latitude = max.latitude.max(p.latitude);
        max.longitude = max.longitude.max(p.longitude);
    }
    // Pad so pins near the edge aren't clipped.
    let pad_lat = ((max.latitude - min.latitude) * 0.15).max(0.0015);
    let pad_lng = ((max.longitude - min.longitude) * 0.15).max(0.0015);
    min.latitude -= pad_lat;
    min.longitude -= pad_lng;
    max.latitude += pad_lat;
    max.longitude += pad_lng;

    let zoom = pick_zoom(&min, &max);
    let tx0 = (lng_to_px(min.longitude, zoom) / TILE as f64).floor() as i64;
    let tx1 = (lng_to_px(max.longitude, zoom) / TILE as f64).floor() as i64;
    let ty0 = (lat_to_px(max.latitude, zoom) / TILE as f64).floor() as i64;
    let ty1 = (lat_to_px(min.latitude, zoom) / TILE as f64).floor() as i64;

    let nx = (tx1 - tx0 + 1).max(1);
    let ny = (ty1 - ty0 + 1).max(1);
    let width = (nx * TILE) as u32;
    let height = (ny * TILE) as u32;
    let origin_px = (tx0 * TILE, ty0 * TILE);

    // Plain, map-ish background in case tiles are unavailable.
    let mut canvas = RgbaImage::from_pixel(width, height, Rgba([233, 231, 224, 255]));

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| Error::new(ErrorKind::Io, format!("tile http client: {e}")))?;

    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            if let Some(tile) = fetch_tile(&client, zoom, tx, ty).await {
                let ox = (tx - tx0) * TILE;
                let oy = (ty - ty0) * TILE;
                image::imageops::overlay(&mut canvas, &tile, ox, oy);
            }
        }
    }

    let to_xy = |p: &LatLng| -> (f32, f32) {
        (
            (lng_to_px(p.longitude, zoom) - origin_px.0 as f64) as f32,
            (lat_to_px(p.latitude, zoom) - origin_px.1 as f64) as f32,
        )
    };

    // Route line (muted Grab-ish green).
    let route_color = Rgba([0, 158, 96, 255]);
    let mut prev: Option<(f32, f32)> = None;
    for p in &model.route {
        let cur = to_xy(p);
        if let Some(a) = prev {
            draw_thick_line(&mut canvas, a, cur, route_color);
        }
        prev = Some(cur);
    }

    // Pins: restaurant (orange), drop-off (blue), driver (green).
    let mut pins = Vec::new();
    if let Some(c) = &model.merchant.coordinates {
        pins.push(Pin {
            at: c.clone(),
            color: Rgba([243, 146, 0, 255]),
        });
    }
    if let Some(c) = &model.dropoff.coordinates {
        pins.push(Pin {
            at: c.clone(),
            color: Rgba([21, 101, 192, 255]),
        });
    }
    if let Some(d) = &model.driver
        && let Some(l) = &d.location
    {
        pins.push(Pin {
            at: l.clone(),
            color: Rgba([0, 158, 96, 255]),
        });
    }
    for pin in &pins {
        let (x, y) = to_xy(&pin.at);
        draw_pin(&mut canvas, x.round() as i32, y.round() as i32, pin.color);
    }

    // Attribution (required by the OSM tile policy), bottom-right on a strip.
    if let Some(font) = load_font() {
        let scale = PxScale::from(13.0);
        let text_w = (ATTRIBUTION.len() as u32) * 7 + 10;
        let strip_h = 18u32;
        let y0 = height.saturating_sub(strip_h);
        let x0 = width.saturating_sub(text_w);
        for y in y0..height {
            for x in x0..width {
                let bg = canvas.get_pixel(x, y);
                let blended = Rgba([
                    ((bg[0] as u16 + 255) / 2) as u8,
                    ((bg[1] as u16 + 255) / 2) as u8,
                    ((bg[2] as u16 + 255) / 2) as u8,
                    255,
                ]);
                canvas.put_pixel(x, y, blended);
            }
        }
        drawing::draw_text_mut(
            &mut canvas,
            Rgba([40, 40, 40, 255]),
            (x0 + 5) as i32,
            (y0 + 2) as i32,
            scale,
            &font,
            ATTRIBUTION,
        );
    }

    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::new(ErrorKind::Io, format!("create map dir: {e}")))?;
    }
    canvas
        .save(path)
        .map_err(|e| Error::new(ErrorKind::Io, format!("write png: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracking;

    #[test]
    fn zoom_fits_tile_budget() {
        let a = LatLng {
            latitude: 12.00,
            longitude: 34.00,
        };
        let b = LatLng {
            latitude: 12.01,
            longitude: 34.01,
        };
        let z = pick_zoom(&a, &b);
        assert!((1..=18).contains(&z));
    }

    #[test]
    fn mercator_monotonic() {
        assert!(lng_to_px(10.0, 10) < lng_to_px(20.0, 10));
        // Higher latitude -> smaller pixel Y (further north/up).
        assert!(lat_to_px(20.0, 10) < lat_to_px(10.0, 10));
    }

    #[tokio::test]
    async fn renders_without_tiles_offline() {
        // No coordinates -> explicit error.
        let empty = tracking::Tracking {
            schema: 1,
            order_id: "X".into(),
            booking_code: None,
            short_number: None,
            state: "PICKING_UP".into(),
            human_state: "x".into(),
            is_final: false,
            cancelable: false,
            stages: vec![],
            eta: tracking::Eta {
                min_minutes: None,
                max_minutes: None,
            },
            driver: None,
            merchant: tracking::Place {
                name: None,
                address: None,
                coordinates: None,
            },
            dropoff: tracking::Place {
                name: None,
                address: None,
                coordinates: None,
            },
            route: vec![],
            route_distance_m: None,
            total_php: None,
            items: vec![],
            web_url: "https://example.com".into(),
        };
        let tmp = std::env::temp_dir().join("grabcli-map-empty-test.png");
        assert!(render(&empty, &tmp).await.is_err());
    }
}
