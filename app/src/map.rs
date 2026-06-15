//! In-app OSM "slippy" map for the Track tab: Web Mercator projection,
//! visible-tile selection, auto-fit, panning, and tile fetching/decoding.
//!
//! The projection/tile math is pure and host-tested. Tiles are raster PNGs
//! from the public OpenStreetMap tile server, fetched over the *same*
//! rustls/ring TLS stack the relay layer already uses (so no new crypto or
//! cross-language dependency is introduced) and decoded with the `image`
//! crate. OSM's tile-usage policy requires a descriptive `User-Agent` and
//! on-screen attribution; the latter is shown by the `MapOverlay` UI.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use slint::{Rgba8Pixel, SharedPixelBuffer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// Side length of an OSM tile, in (logical) pixels.
pub const TILE_SIZE: f64 = 256.0;
/// Zoom range we allow. `MIN` shows roughly a continent; `MAX` ~ street level.
pub const MIN_ZOOM: u32 = 2;
pub const MAX_ZOOM: u32 = 18;
/// Web Mercator is only defined up to this latitude.
const MAX_LAT: f64 = 85.051_128_779_806_59;
/// Cap on cached tiles (≈ a few screenfuls). Oldest-inserted are evicted.
const CACHE_CAP: usize = 256;

const TILE_HOST: &str = "tile.openstreetmap.org";
/// OSM's usage policy requires a valid, identifying `User-Agent`.
pub const USER_AGENT: &str =
    concat!("ntrack/", env!("CARGO_PKG_VERSION"), " (+https://github.com/f321x/ntrack)");

/// An OSM tile coordinate (`z/x/y`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileId {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// A tile placed in the viewport: `dx`/`dy` is its top-left corner's offset
/// (px) from the viewport centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub id: TileId,
    pub dx: f64,
    pub dy: f64,
}

/// World-pixel extent at zoom `z` (`256 · 2^z`).
fn world_size(z: u32) -> f64 {
    TILE_SIZE * f64::from(1u32 << z)
}

/// Project lat/lng (degrees) to world-pixel coordinates at zoom `z`.
pub fn project(lat: f64, lng: f64, z: u32) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LAT, MAX_LAT);
    let s = world_size(z);
    let x = (lng + 180.0) / 360.0 * s;
    let sin = lat.to_radians().sin();
    let y = (0.5 - ((1.0 + sin) / (1.0 - sin)).ln() / (4.0 * std::f64::consts::PI)) * s;
    (x, y)
}

/// Inverse of [`project`].
pub fn unproject(x: f64, y: f64, z: u32) -> (f64, f64) {
    let s = world_size(z);
    let lng = x / s * 360.0 - 180.0;
    let n = std::f64::consts::PI * (1.0 - 2.0 * y / s);
    let lat = n.sinh().atan().to_degrees();
    (lat, lng)
}

/// Tiles covering a `vw`×`vh` viewport centred at (`lat`,`lng`) at zoom `z`,
/// plus a `margin` (px) overscan so a little panning reveals loaded tiles.
/// Vertically out-of-range rows are skipped; columns wrap around the globe.
pub fn visible_tiles(lat: f64, lng: f64, z: u32, vw: f64, vh: f64, margin: f64) -> Vec<Placement> {
    let (cx, cy) = project(lat, lng, z);
    let half_w = vw / 2.0 + margin;
    let half_h = vh / 2.0 + margin;
    let min_tx = ((cx - half_w) / TILE_SIZE).floor() as i64;
    let max_tx = ((cx + half_w) / TILE_SIZE).floor() as i64;
    let min_ty = ((cy - half_h) / TILE_SIZE).floor() as i64;
    let max_ty = ((cy + half_h) / TILE_SIZE).floor() as i64;
    let n = 1i64 << z;
    let mut out = Vec::new();
    for ty in min_ty..=max_ty {
        if ty < 0 || ty >= n {
            continue; // no vertical wrap
        }
        for tx in min_tx..=max_tx {
            let id = TileId {
                z,
                x: tx.rem_euclid(n) as u32, // horizontal wrap
                y: ty as u32,
            };
            out.push(Placement {
                id,
                dx: tx as f64 * TILE_SIZE - cx,
                dy: ty as f64 * TILE_SIZE - cy,
            });
        }
    }
    out
}

/// Offset (px) of a point from the viewport centre at zoom `z`.
pub fn marker_offset(center_lat: f64, center_lng: f64, lat: f64, lng: f64, z: u32) -> (f64, f64) {
    let (cx, cy) = project(center_lat, center_lng, z);
    let (px, py) = project(lat, lng, z);
    (px - cx, py - cy)
}

/// New centre after the user drags the map content by (`dx`,`dy`) px.
pub fn pan(center_lat: f64, center_lng: f64, z: u32, dx: f64, dy: f64) -> (f64, f64) {
    let (cx, cy) = project(center_lat, center_lng, z);
    unproject(cx - dx, cy - dy, z)
}

/// Centre + zoom framing all `points` (lat,lng) inside a `vw`×`vh` viewport.
/// Empty → a world view; a single point → a sensible street-level zoom.
pub fn fit(points: &[(f64, f64)], vw: f64, vh: f64) -> (f64, f64, u32) {
    match points {
        [] => return (20.0, 0.0, MIN_ZOOM),
        [(lat, lng)] => return (*lat, *lng, 14),
        _ => {}
    }
    let mut min_lat = 90.0f64;
    let mut max_lat = -90.0f64;
    let mut min_lng = 180.0f64;
    let mut max_lng = -180.0f64;
    for &(lat, lng) in points {
        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lng = min_lng.min(lng);
        max_lng = max_lng.max(lng);
    }
    let center_lat = (min_lat + max_lat) / 2.0;
    let center_lng = (min_lng + max_lng) / 2.0;
    // Leave a margin so dots aren't flush against the edge.
    let budget_w = (vw * 0.8).max(64.0);
    let budget_h = (vh * 0.8).max(64.0);
    let mut zoom = MIN_ZOOM;
    for z in (MIN_ZOOM..=MAX_ZOOM).rev() {
        let (x0, y0) = project(max_lat, min_lng, z); // top-left
        let (x1, y1) = project(min_lat, max_lng, z); // bottom-right
        if (x1 - x0).abs() <= budget_w && (y1 - y0).abs() <= budget_h {
            zoom = z;
            break;
        }
    }
    (center_lat, center_lng, zoom)
}

/// Cache slot for one tile.
pub enum TileSlot {
    /// A fetch is in flight.
    Loading,
    /// Decoded RGBA pixels, ready to wrap in a `slint::Image` at render time.
    Loaded(SharedPixelBuffer<Rgba8Pixel>),
    /// Fetch or decode failed; not retried until the map is reopened.
    Failed,
}

/// View + tile cache backing the map overlay. Lives inside the controller's
/// `ViewState` (behind its mutex); fetch tasks update the cache, `render`
/// reads it.
pub struct MapState {
    pub open: bool,
    pub center_lat: f64,
    pub center_lng: f64,
    pub zoom: u32,
    /// Last-reported viewport size (px); seeded with a phone-ish default so we
    /// fetch a sensible set even before the UI reports its real geometry.
    pub vw: f64,
    pub vh: f64,
    tiles: HashMap<TileId, TileSlot>,
    /// Insertion order, for FIFO eviction past [`CACHE_CAP`].
    order: VecDeque<TileId>,
}

impl Default for MapState {
    fn default() -> Self {
        Self {
            open: false,
            center_lat: 20.0,
            center_lng: 0.0,
            zoom: MIN_ZOOM,
            vw: 400.0,
            vh: 800.0,
            tiles: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl MapState {
    pub fn get(&self, id: &TileId) -> Option<&TileSlot> {
        self.tiles.get(id)
    }

    pub fn contains(&self, id: &TileId) -> bool {
        self.tiles.contains_key(id)
    }

    /// Insert or replace a tile, evicting the oldest entries past the cap.
    pub fn insert(&mut self, id: TileId, slot: TileSlot) {
        if self.tiles.insert(id, slot).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.tiles.remove(&old);
            }
        }
    }

    /// Drop everything except successfully-loaded tiles. Called on (re)open so
    /// stale `Loading`/`Failed` entries don't wedge or hide tiles, while loaded
    /// imagery survives for an instant redraw.
    pub fn retain_loaded(&mut self) {
        self.tiles.retain(|_, s| matches!(s, TileSlot::Loaded(_)));
        self.order.retain(|id| self.tiles.contains_key(id));
    }
}

/// Build the shared client TLS config (rustls + ring, Mozilla webpki roots),
/// reusing the process-wide crypto provider the relay layer installs.
pub fn tls_config() -> Arc<ClientConfig> {
    ntrack_core::relay::ensure_crypto_provider();
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Fetch and decode one OSM tile. `None` on any network/HTTP/decode error.
pub async fn fetch_tile(
    tls: Arc<ClientConfig>,
    id: TileId,
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let path = format!("/{}/{}/{}.png", id.z, id.x, id.y);
    let body = https_get(tls, TILE_HOST, &path).await?;
    decode_png(&body)
}

/// Minimal HTTPS GET: open one TLS connection, send an HTTP/1.1 request with
/// `Connection: close`, read the response to EOF and return the (de-chunked)
/// body. Sufficient for fetching static tiles from a CDN; not a general client.
async fn https_get(tls: Arc<ClientConfig>, host: &str, path: &str) -> Option<Vec<u8>> {
    let stream = TcpStream::connect((host, 443)).await.ok()?;
    let domain = ServerName::try_from(host.to_string()).ok()?;
    let connector = TlsConnector::from(tls);
    let mut tls_stream = connector.connect(domain, stream).await.ok()?;

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {USER_AGENT}\r\n\
         Accept: image/png\r\n\
         Connection: close\r\n\r\n"
    );
    tls_stream.write_all(request.as_bytes()).await.ok()?;
    tls_stream.flush().await.ok()?;

    // Bound memory in case a server misbehaves (a tile is ~5–40 KB).
    let mut raw = Vec::new();
    tls_stream
        .take(2 * 1024 * 1024)
        .read_to_end(&mut raw)
        .await
        .ok()?;
    parse_http_body(&raw)
}

/// Split an HTTP/1.1 response into status/headers/body, returning the body
/// only on `200`, de-chunking when `Transfer-Encoding: chunked`.
fn parse_http_body(raw: &[u8]) -> Option<Vec<u8>> {
    let sep = find(raw, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..sep]).ok()?;
    let body = &raw[sep + 4..];

    let mut lines = head.split("\r\n");
    let status = lines.next()?; // "HTTP/1.1 200 OK"
    if status.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        dechunk(body)
    } else {
        Some(body.to_vec())
    }
}

/// Decode an HTTP/1.1 chunked body.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = find(data, b"\r\n")?;
        let size_line = std::str::from_utf8(&data[..nl]).ok()?;
        let size_hex = size_line.split(';').next()?.trim(); // ignore chunk-ext
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..]; // skip chunk data + trailing CRLF
    }
    Some(out)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_png(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    buf.make_mut_bytes().copy_from_slice(img.as_raw());
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_unproject_roundtrip() {
        for &(lat, lng) in &[(0.0, 0.0), (48.2, 16.37), (-33.87, 151.21), (60.0, -120.0)] {
            for z in [2, 8, 14, 18] {
                let (x, y) = project(lat, lng, z);
                let (lat2, lng2) = unproject(x, y, z);
                assert!((lat - lat2).abs() < 1e-6, "lat {lat} != {lat2} @z{z}");
                assert!((lng - lng2).abs() < 1e-6, "lng {lng} != {lng2} @z{z}");
            }
        }
    }

    #[test]
    fn project_origin_is_world_centre() {
        // (0,0) maps to the middle of the world square at every zoom.
        for z in [0u32, 4, 10] {
            let (x, y) = project(0.0, 0.0, z);
            assert!((x - world_size(z) / 2.0).abs() < 1e-6);
            assert!((y - world_size(z) / 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn latitude_is_clamped_to_mercator_limit() {
        // Beyond the Mercator limit the projection stays finite (clamped, not
        // ±inf): the far north pins to the top of the map (y≈0), the far south
        // to the bottom (y≈world_size).
        let s = world_size(5);
        let (_, y_north) = project(89.9, 0.0, 5);
        let (_, y_south) = project(-89.9, 0.0, 5);
        assert!(y_north.is_finite() && y_south.is_finite());
        assert!(y_north < 1.0, "north y={y_north}");
        assert!(y_south > s - 1.0, "south y={y_south}");
    }

    #[test]
    fn visible_tiles_cover_viewport_and_offsets_are_consistent() {
        let tiles = visible_tiles(0.0, 0.0, 4, 512.0, 512.0, 0.0);
        assert!(!tiles.is_empty());
        // Centre tile coordinates are valid and offsets land near the centre.
        let n = 1i64 << 4;
        for p in &tiles {
            assert!((p.id.x as i64) < n && (p.id.y as i64) < n);
            // A tile's top-left offset is a whole number of tiles from centre.
            let rem = (p.dx.rem_euclid(TILE_SIZE)).min((TILE_SIZE - p.dx.rem_euclid(TILE_SIZE)).abs());
            assert!(rem < 1e-6, "dx not tile-aligned: {}", p.dx);
        }
    }

    #[test]
    fn visible_tiles_skip_rows_outside_the_world() {
        // Near the north edge at low zoom, no negative tile rows are emitted.
        let tiles = visible_tiles(MAX_LAT, 0.0, 2, 1024.0, 1024.0, 256.0);
        assert!(tiles.iter().all(|p| p.id.y < (1 << 2)));
    }

    #[test]
    fn pan_then_unpan_returns_to_start() {
        let (lat, lng) = (48.2, 16.37);
        let (l2, g2) = pan(lat, lng, 12, 100.0, -60.0);
        let (l3, g3) = pan(l2, g2, 12, -100.0, 60.0);
        assert!((lat - l3).abs() < 1e-9 && (lng - g3).abs() < 1e-9);
    }

    #[test]
    fn pan_moves_centre_in_the_expected_direction() {
        // Dragging content right (+dx) reveals area to the west → centre lng drops.
        let (_, lng) = pan(0.0, 0.0, 10, 50.0, 0.0);
        assert!(lng < 0.0);
        // Dragging content down (+dy) reveals area to the north → centre lat rises.
        let (lat, _) = pan(0.0, 0.0, 10, 0.0, 50.0);
        assert!(lat > 0.0);
    }

    #[test]
    fn fit_handles_empty_and_single() {
        assert_eq!(fit(&[], 400.0, 800.0), (20.0, 0.0, MIN_ZOOM));
        let (lat, lng, z) = fit(&[(48.2, 16.37)], 400.0, 800.0);
        assert_eq!((lat, lng, z), (48.2, 16.37, 14));
    }

    #[test]
    fn fit_picks_higher_zoom_for_closer_points() {
        let near = fit(&[(48.20, 16.37), (48.21, 16.38)], 400.0, 800.0);
        let far = fit(&[(48.2, 16.37), (40.7, -74.0)], 400.0, 800.0);
        assert!(near.2 > far.2, "closer points should zoom in more");
        assert!((MIN_ZOOM..=MAX_ZOOM).contains(&near.2));
        assert!((MIN_ZOOM..=MAX_ZOOM).contains(&far.2));
    }

    #[test]
    fn cache_evicts_oldest_past_cap() {
        let mut m = MapState::default();
        for i in 0..(CACHE_CAP as u32 + 10) {
            m.insert(TileId { z: 10, x: i, y: 0 }, TileSlot::Failed);
        }
        assert!(m.tiles.len() <= CACHE_CAP);
        // The very first inserts are gone; the latest survive.
        assert!(!m.contains(&TileId { z: 10, x: 0, y: 0 }));
        assert!(m.contains(&TileId { z: 10, x: CACHE_CAP as u32 + 9, y: 0 }));
    }

    #[test]
    fn retain_loaded_drops_loading_and_failed() {
        let mut m = MapState::default();
        m.insert(TileId { z: 1, x: 0, y: 0 }, TileSlot::Loading);
        m.insert(TileId { z: 1, x: 1, y: 0 }, TileSlot::Failed);
        m.insert(
            TileId { z: 1, x: 1, y: 1 },
            TileSlot::Loaded(SharedPixelBuffer::new(1, 1)),
        );
        m.retain_loaded();
        assert!(!m.contains(&TileId { z: 1, x: 0, y: 0 }));
        assert!(!m.contains(&TileId { z: 1, x: 1, y: 0 }));
        assert!(m.contains(&TileId { z: 1, x: 1, y: 1 }));
        assert_eq!(m.order.len(), 1);
    }

    #[test]
    fn parse_body_rejects_non_200() {
        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\n\r\nno!";
        assert_eq!(parse_http_body(resp), None);
    }

    #[test]
    fn parse_body_plain() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_http_body(resp).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn parse_body_chunked() {
        // "Wikipedia" in two chunks, per the RFC 7230 example shape.
        let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(parse_http_body(resp).as_deref(), Some(&b"Wikipedia"[..]));
    }
}
