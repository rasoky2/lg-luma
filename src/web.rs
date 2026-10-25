//! On-demand LAN configuration panel for Luma.
//!
//! Starts a tiny HTTP server (no external crates) that serves a mobile friendly
//! page to pick which apps appear on the Home screen and to replace the current
//! wallpaper with an image (URL or upload). The server is meant to be started
//! from Settings -> "Web panel / QR", shows a QR on the TV and stops itself
//! after a period without requests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const PORT: u16 = 8090;
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_BODY: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Paths {
    pub preferences: PathBuf,
    pub app_dir: PathBuf,
    pub staged_splash: PathBuf,
}

impl Paths {
    pub fn webos() -> Self {
        Self {
            preferences: PathBuf::from("/media/developer/com.ecklf.luma/preferences.json"),
            app_dir: PathBuf::from("/media/developer/apps/usr/palm/applications/com.ecklf.luma"),
            staged_splash: PathBuf::from("/var/lib/luma-home/app/splash.png"),
        }
    }
}

pub struct Panel {
    url: String,
    matrix: Vec<bool>,
    size: usize,
    stop: Arc<AtomicBool>,
}

impl Panel {
    pub fn url(&self) -> &str {
        &self.url
    }

    /// QR matrix as a flat `size * size` row-major boolean slice.
    pub fn matrix(&self) -> (&[bool], usize) {
        (&self.matrix, self.size)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(IpAddr::V4(v4)),
        _ => None,
    }
}

fn token() -> String {
    // xorshift seeded from the clock: good enough for a short-lived LAN token.
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
        | 1;
    let alphabet = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut out = String::with_capacity(10);
    for _ in 0..10 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(alphabet[(x % alphabet.len() as u64) as usize] as char);
    }
    out
}

fn qr_matrix(text: &str) -> Option<(Vec<bool>, usize)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 8; // quiet zone
    let mut matrix = vec![false; size * size];
    for (index, color) in colors.iter().enumerate() {
        let x = index % width;
        let y = index / width;
        matrix[(y + 4) * size + x + 4] = *color == qrcode::Color::Dark;
    }
    Some((matrix, size))
}

pub fn start(paths: Paths, apps: Vec<(String, String)>) -> Result<Panel, String> {
    note("panel: starting");
    let ip = local_ip().ok_or("no LAN address found")?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, PORT))
        .map_err(|e| format!("cannot bind port {PORT}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener setup failed: {e}"))?;

    let token = token();
    let url = format!("http://{ip}:{PORT}/?k={token}");
    let (matrix, size) = qr_matrix(&url).ok_or("cannot build QR code")?;

    let stop = Arc::new(AtomicBool::new(false));

    let worker = Worker {
        paths,
        apps,
        token,
        stop: Arc::clone(&stop),
    };
    thread::spawn(move || worker.run(listener));
    note(&format!("panel: listening on {url}"));

    Ok(Panel {
        url,
        matrix,
        size,
        stop,
    })
}

struct Worker {
    paths: Paths,
    apps: Vec<(String, String)>,
    token: String,
    stop: Arc<AtomicBool>,
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

impl Worker {
    fn run(&self, listener: TcpListener) {
        let mut last = Instant::now();
        while !self.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    last = Instant::now();
                    let _ = self.handle(stream);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if last.elapsed() > IDLE_TIMEOUT {
                        break;
                    }
                    thread::sleep(Duration::from_millis(120));
                }
                Err(_) => break,
            }
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        stream.set_write_timeout(Some(Duration::from_secs(20)))?;
        let request = match read_request(&mut stream) {
            Some(r) => r,
            None => return respond(&mut stream, 400, "text/plain", b"bad request"),
        };

        let authorized = query_value(&request.query, "k").as_deref() == Some(self.token.as_str());
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") if authorized => respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                PAGE.as_bytes(),
            ),
            ("GET", "/api/apps") if authorized => {
                let body = self.apps_payload();
                respond(&mut stream, 200, "application/json", body.as_bytes())
            }
            ("POST", "/api/apps") if authorized => match self.save_apps(&request.body) {
                Ok(message) => respond(
                    &mut stream,
                    200,
                    "application/json",
                    json_message(&message, true).as_bytes(),
                ),
                Err(message) => respond(
                    &mut stream,
                    400,
                    "application/json",
                    json_message(&message, false).as_bytes(),
                ),
            },
            ("POST", "/api/wallpaper") if authorized => match self.save_wallpaper(&request) {
                Ok(message) => respond(
                    &mut stream,
                    200,
                    "application/json",
                    json_message(&message, true).as_bytes(),
                ),
                Err(message) => respond(
                    &mut stream,
                    400,
                    "application/json",
                    json_message(&message, false).as_bytes(),
                ),
            },
            ("POST", "/api/restart") if authorized => {
                restart_home();
                respond(
                    &mut stream,
                    200,
                    "application/json",
                    json_message("Reiniciando Luma", true).as_bytes(),
                )
            }
            ("GET", "/favicon.ico") => respond(&mut stream, 204, "text/plain", b""),
            _ => respond(&mut stream, 403, "text/plain", b"forbidden"),
        }
    }

    fn save_apps(&self, body: &[u8]) -> Result<String, String> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| format!("JSON invalido: {e}"))?;
        let favorites: Vec<String> = value
            .get("favorites")
            .and_then(|v| v.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str())
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| id.trim().to_string())
                    .collect()
            })
            .ok_or("falta el campo favorites")?;
        if favorites.is_empty() {
            return Err("selecciona al menos una app".into());
        }
        let text = std::fs::read_to_string(&self.paths.preferences)
            .map_err(|e| format!("no se pudo leer preferences.json: {e}"))?;
        let mut prefs: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("preferences.json invalido: {e}"))?;
        prefs["favorites"] = serde_json::json!(favorites);
        let out = serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?;
        std::fs::write(&self.paths.preferences, out)
            .map_err(|e| format!("no se pudo escribir preferences.json: {e}"))?;
        Ok(format!("Guardadas {} apps", favorites.len()))
    }

    fn save_wallpaper(&self, request: &Request) -> Result<String, String> {
        let slot = query_value(&request.query, "slot").unwrap_or_else(|| "northern-lights".into());
        let valid = [
            "slate",
            "midnight",
            "prism",
            "dunes",
            "san-francisco",
            "summit",
            "forest",
            "yosemite",
            "northern-lights",
        ];
        if !valid.contains(&slot.as_str()) {
            return Err("slot invalido".into());
        }

        let bytes = if request.body.is_empty() {
            let value: serde_json::Value =
                serde_json::from_slice(&request.body).map_err(|e| e.to_string())?;
            let url = value
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("falta url o cuerpo de imagen")?;
            fetch_url(url)?
        } else {
            request.body.clone()
        };
        if bytes.is_empty() {
            return Err("imagen vacia".into());
        }
        if bytes.len() > MAX_BODY {
            return Err("imagen demasiado grande".into());
        }

        let image = image::load_from_memory(&bytes)
            .map_err(|e| format!("formato de imagen no soportado: {e}"))?
            .to_rgb8();
        let framed = fit_16_9(image);
        let resized =
            image::imageops::resize(&framed, 1920, 1080, image::imageops::FilterType::Lanczos3);

        let walls = self.paths.app_dir.join("assets/wallpapers");
        let splashes = self.paths.app_dir.join("splash");
        std::fs::create_dir_all(&walls).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&splashes).map_err(|e| e.to_string())?;

        let jpg = walls.join(format!("{slot}.jpg"));
        let png = splashes.join(format!("{slot}.png"));
        backup(&jpg);
        backup(&png);

        let file = std::fs::File::create(&jpg).map_err(|e| format!("no se pudo crear jpg: {e}"))?;
        let mut enc =
            image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 90);
        enc.encode_image(&resized)
            .map_err(|e| format!("no se pudo codificar jpg: {e}"))?;

        resized
            .save_with_format(&png, image::ImageFormat::Png)
            .map_err(|e| format!("no se pudo guardar png: {e}"))?;

        let _ = std::fs::copy(&png, &self.paths.staged_splash);
        let _ = set_background(&self.paths.preferences, &slot);
        Ok(format!("Fondo actualizado ({slot})"))
    }
}

fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("la URL debe empezar por http:// o https://".into());
    }
    let mut response = ureq::get(url)
        .call()
        .map_err(|e| format!("descarga fallida: {e}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("lectura fallida: {e}"))?;
    if bytes.len() > MAX_BODY {
        return Err("la imagen supera 24 MB".into());
    }
    Ok(bytes)
}

fn fit_16_9(image: image::RgbImage) -> image::RgbImage {
    let (w, h) = image.dimensions();
    let target = 1920.0 / 1080.0;
    let ratio = w as f32 / h as f32;
    if (ratio - target).abs() < 0.001 {
        return image;
    }
    if ratio > target {
        let new_w = (h as f32 * target).round() as u32;
        let x = (w - new_w) / 2;
        image::imageops::crop_imm(&image, x, 0, new_w, h).to_image()
    } else {
        let new_h = (w as f32 / target).round() as u32;
        let y = (h - new_h) / 2;
        image::imageops::crop_imm(&image, 0, y, w, new_h).to_image()
    }
}

fn backup(path: &Path) {
    let bak = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|e| e.to_str()).unwrap_or("img")
    ));
    if path.exists() && !bak.exists() {
        let _ = std::fs::copy(path, bak);
    }
}

fn set_background(preferences: &Path, slot: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(preferences).map_err(|e| e.to_string())?;
    let mut prefs: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    prefs["background"] = serde_json::json!(slot);
    std::fs::write(
        preferences,
        serde_json::to_string_pretty(&prefs).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn note(message: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/luma-panel.log")
    {
        let _ = writeln!(file, "{message}");
    }
}

fn restart_home() {
    note("panel: restarting Home");
    let _ = Command::new("pkill").args(["-x", "luma"]).status();
    thread::sleep(Duration::from_millis(1500));
    for binary in ["/usr/bin/luna-send", "luna-send"] {
        match Command::new(binary)
            .args([
                "-n",
                "1",
                "luna://com.webos.applicationManager/launch",
                "{\"id\":\"com.webos.app.home\"}",
            ])
            .status()
        {
            Ok(status) => {
                note(&format!("panel: launch via {binary} -> {status}"));
                break;
            }
            Err(err) => note(&format!("panel: {binary} failed: {err}")),
        }
    }
}

impl Worker {
    fn apps_payload(&self) -> String {
        let favorites: Vec<String> = std::fs::read_to_string(&self.paths.preferences)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("favorites")
                    .and_then(|v| v.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str())
                            .map(str::to_string)
                            .collect()
                    })
            })
            .unwrap_or_default();
        let apps: Vec<serde_json::Value> = self
            .apps
            .iter()
            .map(|(id, title)| serde_json::json!({"id": id, "title": title}))
            .collect();
        serde_json::json!({ "apps": apps, "favorites": favorites }).to_string()
    }
}

fn json_message(message: &str, ok: bool) -> String {
    serde_json::json!({ "ok": ok, "message": message }).to_string()
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end;
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = find_header_end(&buffer) {
            header_end = pos;
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut content_length = 0usize;
    let mut chunked = false;
    let mut expect_continue = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.to_ascii_lowercase().contains("chunked");
            } else if name.eq_ignore_ascii_case("expect") {
                expect_continue = value.to_ascii_lowercase().contains("100-continue");
            }
        }
    }
    if content_length > MAX_BODY {
        return None;
    }

    if expect_continue {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = stream.flush();
    }

    let leftover = buffer[header_end + 4..].to_vec();
    let mut reader = BufReader::new(std::io::Cursor::new(leftover).chain(stream));
    let body = if chunked {
        read_chunked(&mut reader)?
    } else if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).ok()?;
        body
    } else {
        Vec::new()
    };
    if body.len() > MAX_BODY {
        return None;
    }

    Some(Request {
        method,
        path,
        query,
        body,
    })
}

fn read_chunked<R: Read>(reader: &mut BufReader<R>) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let hex = line.trim().split(';').next()?.trim();
        let size = usize::from_str_radix(hex, 16).ok()?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                let read = reader.read_line(&mut trailer).ok()?;
                if read == 0 || trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if body.len() + size > MAX_BODY {
            return None;
        }
        let mut part = vec![0u8; size];
        reader.read_exact(&mut part).ok()?;
        body.extend_from_slice(&part);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).ok()?;
    }
    Some(body)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(percent_decode(value))
        } else {
            None
        }
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

const PAGE: &str = r#"<!doctype html>
<html lang="es"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Luma Ã‚Â· Configuracion</title>
<style>
:root{color-scheme:dark}
body{margin:0;font:16px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif;background:#14161a;color:#f2f3f5}
header{padding:20px 18px 8px}h1{font-size:20px;margin:0 0 4px}p.sub{margin:0;color:#9aa0a6;font-size:13px}
main{padding:8px 18px 40px;max-width:760px;margin:0 auto}
section{background:#1c1f24;border-radius:14px;padding:16px;margin:16px 0}
h2{font-size:15px;margin:0 0 10px;color:#cfd3d8;text-transform:uppercase;letter-spacing:.06em}
label.app{display:flex;align-items:center;gap:10px;padding:10px 6px;border-radius:10px}
label.app:active{background:#262a30}
label.app input{width:20px;height:20px;accent-color:#4f8cff}
input[type=url]{width:100%;padding:12px;border-radius:10px;border:1px solid #333a42;background:#0f1114;color:#fff;box-sizing:border-box}
button{margin-top:12px;padding:12px 16px;border:0;border-radius:10px;background:#4f8cff;color:#fff;font-weight:600;font-size:15px;width:100%}
button.ghost{background:#2a2f36}
input[type=file]{margin-top:10px;color:#cfd3d8}
#msg{margin-top:14px;font-size:14px;min-height:20px;color:#7ee787}
#msg.err{color:#ff7b72}
#count{color:#9aa0a6;font-size:13px;margin-top:8px}
</style></head><body>
<header><h1>Luma Ã‚Â· Configuracion</h1><p class="sub">Conexion directa con la TV. Cierra esta pagina al terminar.</p></header>
<main>
<section>
<h2>Apps del inicio</h2>
<div id="apps">Cargando...</div>
<div id="count"></div>
<button onclick="saveApps()">Guardar apps</button>
</section>
<section>
<h2>Fondo de pantalla</h2>
<p class="sub">URL de imagen (jpg/png/webp) o subir un archivo. Se recorta a 16:9 (1920x1080).</p>
<input id="url" type="url" placeholder="https://ejemplo.com/fondo.jpg">
<button onclick="setUrl()">Usar esta URL</button>
<input id="file" type="file" accept="image/*">
<button class="ghost" onclick="upload()">Subir archivo</button>
</section>
<section>
<h2>Reiniciar</h2>
<button class="ghost" onclick="restart()">Reiniciar Luma</button>
</section>
<div id="msg"></div>
</main>
<script>
const k = new URLSearchParams(location.search).get('k');
const msg = document.getElementById('msg');
function say(t, err){ msg.textContent = t; msg.className = err ? 'err' : ''; }
async function api(path, opts){
  const sep = path.includes('?') ? '&' : '?';
  const r = await fetch(path + sep + 'k=' + encodeURIComponent(k), opts);
  const t = await r.text();
  try { return JSON.parse(t); } catch { return {ok:r.ok, message:t}; }
}
async function load(){
  const data = await api('/api/apps');
  const box = document.getElementById('apps');
  if(!data.apps || !data.apps.length){ box.textContent = 'No se pudieron leer las apps.'; return; }
  box.innerHTML = '';
  data.apps.sort((a,b)=>a.title.localeCompare(b.title)).forEach(app=>{
    const l = document.createElement('label'); l.className='app';
    const c = document.createElement('input'); c.type='checkbox'; c.value=app.id; if((data.favorites||[]).indexOf(app.id)>=0) c.checked = true;
    const s = document.createElement('span'); s.textContent = app.title || app.id;
    l.appendChild(c); l.appendChild(s); box.appendChild(l);
    c.addEventListener('change', counter);
  });
  counter();
}
function selected(){ return [...document.querySelectorAll('#apps input:checked')].map(c=>c.value); }
function counter(){ document.getElementById('count').textContent = selected().length + ' seleccionadas'; }
async function saveApps(){
  const favorites = selected();
  if(!favorites.length){ say('Elige al menos una app.', true); return; }
  const r = await api('/api/apps', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({favorites})});
  say(r.message, !r.ok);
}
async function setUrl(){
  const url = document.getElementById('url').value.trim();
  if(!url){ say('Escribe una URL.', true); return; }
  say('Descargando...');
  const r = await api('/api/wallpaper', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({url})});
  say(r.message, !r.ok);
}
async function upload(){
  const f = document.getElementById('file').files[0];
  if(!f){ say('Elige un archivo.', true); return; }
  say('Subiendo...');
  const buf = await f.arrayBuffer();
  const r = await api('/api/wallpaper', {method:'POST', headers:{'Content-Type':f.type || 'application/octet-stream'}, body: buf});
  say(r.message, !r.ok);
}
async function restart(){
  const r = await api('/api/restart', {method:'POST'});
  say(r.message || 'Reiniciando', false);
}
load();
</script></body></html>"#;
