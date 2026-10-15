//! tape-deck-rs: YouTube audio downloader with a 16-bit tape deck UI.
//! Rust port of tape-deck (Bun). Same JSON API, same static UI.

mod helpers;

use helpers::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const PORT: u16 = 3030;

// ---------------- jobs ----------------

#[derive(Clone)]
struct Job {
    id: String,
    status: String, // working | done | error
    phase: String,
    progress: u32,
    title: String,
    duration: String,
    filename: String,
    error: String,
    child_pid: Option<u32>,
}

#[derive(Serialize)]
struct JobView<'a> {
    ok: bool,
    id: &'a str,
    status: &'a str,
    phase: &'a str,
    progress: u32,
    title: &'a str,
    duration: &'a str,
    filename: &'a str,
    error: &'a str,
}

impl Job {
    fn view(&self) -> JobView<'_> {
        JobView {
            ok: true,
            id: &self.id,
            status: &self.status,
            phase: &self.phase,
            progress: self.progress,
            title: &self.title,
            duration: &self.duration,
            filename: &self.filename,
            error: &self.error,
        }
    }
}

type Jobs = Arc<Mutex<HashMap<String, Job>>>;

fn job_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), n ^ 0x9e3779b9)[..8.min(16)].to_string()
}

// ---------------- yt-dlp ----------------

fn yt_info(url: &str) -> Result<(String, String, String), String> {
    let out = Command::new("yt-dlp")
        .args([
            "--skip-download",
            "--no-playlist",
            "--extractor-args",
            "youtube:player_client=android",
            "--print",
            "%(title)s",
            "--print",
            "%(duration_string)s",
            "--print",
            "%(id)s",
            url,
        ])
        .output()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if !out.status.success() {
        return Err("could not read video info".to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let title = lines.next().unwrap_or("").trim().to_string();
    if title.is_empty() {
        return Err("no title found".to_string());
    }
    Ok((
        title,
        lines.next().unwrap_or("").trim().to_string(),
        lines.next().unwrap_or("").trim().to_string(),
    ))
}

fn run_download(jobs: Jobs, id: String, url: String, format: String, dl_dir: PathBuf) {
    let args = build_args(&url, &format, &dl_dir.to_string_lossy());
    let mut child = match Command::new("yt-dlp")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let mut m = jobs.lock().unwrap();
            if let Some(j) = m.get_mut(&id) {
                j.status = "error".into();
                j.error = format!("could not start yt-dlp: {e}");
            }
            return;
        }
    };
    let pid = child.id();
    {
        let mut m = jobs.lock().unwrap();
        if let Some(j) = m.get_mut(&id) {
            j.child_pid = Some(pid);
        }
    }

    let stderr = child.stderr.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // stderr carries progress; read it fully (ends when the child exits)
    let mut err_tail: Vec<String> = Vec::new();
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        let t = line.trim().to_string();
        if t.is_empty() {
            continue;
        }
        if let Some((p, phase)) = parse_progress(&t) {
            let mut m = jobs.lock().unwrap();
            if let Some(j) = m.get_mut(&id) {
                j.progress = j.progress.max(p);
                j.phase = match phase {
                    Phase::Downloading => "downloading",
                    Phase::Converting => "converting",
                }
                .into();
            }
        }
        err_tail.push(t);
        if err_tail.len() > 10 {
            err_tail.remove(0);
        }
    }
    // stdout carries the after_move:filepath print
    let mut out_text = String::new();
    BufReader::new(stdout).read_to_string(&mut out_text).ok();
    let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    let mut filename = String::new();
    for line in out_text.lines() {
        let t = line.trim();
        if !t.is_empty()
            && !t.starts_with('[')
            && ["mp3", "m4a", "flac"]
                .iter()
                .any(|e| t.to_lowercase().ends_with(&format!(".{e}")))
        {
            filename = t.rsplit('/').next().unwrap_or("").to_string();
        }
    }

    let mut m = jobs.lock().unwrap();
    if let Some(j) = m.get_mut(&id) {
        j.child_pid = None;
        // a cancel that landed first wins
        let was_cancelled = j.status == "error" && j.error == "cancelled";
        if code == 0 && !filename.is_empty() {
            j.status = "done".into();
            j.progress = 100;
            j.phase = "done".into();
            j.filename = filename;
        } else if code == 0 {
            match newest_tape(&dl_dir) {
                Some(n) => {
                    j.status = "done".into();
                    j.progress = 100;
                    j.phase = "done".into();
                    j.filename = n;
                }
                None => {
                    j.status = "error".into();
                    j.error = "download finished but no file found".into();
                }
            }
        } else if !was_cancelled {
            j.status = "error".into();
            j.error = friendly_error(&err_tail.join("\n"), code);
        }
    }
}

fn newest_tape(dl_dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for e in fs::read_dir(dl_dir).ok()? {
        let e = e.ok()?;
        let name = e.file_name().to_string_lossy().to_string();
        if !safe_name(&name) {
            continue;
        }
        let mt = e.metadata().ok()?.modified().ok()?;
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, name));
        }
    }
    best.map(|(_, n)| n)
}

// ---------------- http helpers ----------------

fn json_body<T>(s: &str) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(s).map_err(|e| format!("bad json: {e}"))
}

fn json_resp<T: Serialize>(v: &T) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn err_json(msg: &str, code: u16) -> Response<Cursor<Vec<u8>>> {
    json_resp(&serde_json::json!({ "ok": false, "error": msg }))
        .with_status_code(StatusCode(code))
}

fn read_body(req: &mut Request) -> String {
    let mut s = String::new();
    req.as_reader().read_to_string(&mut s).ok();
    s
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "webm" => "audio/webm",
        _ => "application/octet-stream",
    }
}

fn serve_file(path: &Path, req: &Request, download: bool) -> Response<Cursor<Vec<u8>>> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(_) => {
            return Response::from_string("not found").with_status_code(StatusCode(404));
        }
    };
    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    // Range support (audio seeking)
    if let Some(range) = req
        .headers()
        .iter()
        .find(|h| h.field.as_str() == "Range")
        .map(|h| h.value.as_str().to_string())
    {
        if let Some((start, end)) = parse_range(&range, data.len() as u64) {
            let slice = data[start as usize..=end as usize].to_vec();
            let mut r = Response::from_data(slice).with_status_code(StatusCode(206));
            r.add_header(Header::from_bytes("Content-Type", content_type(path)).unwrap());
            r.add_header(Header::from_bytes("Accept-Ranges", "bytes").unwrap());
            r.add_header(
                Header::from_bytes(
                    "Content-Range",
                    format!("bytes {start}-{end}/{}", data.len()),
                )
                .unwrap(),
            );
            return r;
        }
    }
    let mut r = Response::from_data(data);
    r.add_header(Header::from_bytes("Content-Type", content_type(path)).unwrap());
    r.add_header(Header::from_bytes("Accept-Ranges", "bytes").unwrap());
    if download {
        r.add_header(
            Header::from_bytes("Content-Disposition", format!("attachment; filename=\"{fname}\""))
                .unwrap(),
        );
    }
    r
}

fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?.trim();
    let (s, e) = spec.split_once('-')?;
    if s.is_empty() {
        // suffix: last N bytes
        let n: u64 = e.parse().ok()?;
        if n == 0 || n > len {
            return None;
        }
        return Some((len - n, len - 1));
    }
    let start: u64 = s.parse().ok()?;
    let end: u64 = if e.is_empty() {
        len - 1
    } else {
        e.parse().ok()?
    };
    if start >= len || end < start {
        return None;
    }
    Some((start, end.min(len - 1)))
}

fn tool_version(cmd: &str) -> Option<String> {
    let out = Command::new(cmd)
        .arg("--version")
        .output()
        .ok()?;
    if out.status.success() {
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !t.is_empty() {
            return t.lines().next().map(|s| s.to_string());
        }
    }
    // fallback: present on PATH at all?
    let w = Command::new("which").arg(cmd).output().ok()?;
    if w.status.success() && !w.stdout.is_empty() {
        return Some("installed".to_string());
    }
    None
}

// ---------------- routes ----------------

#[derive(Deserialize)]
struct UrlBody {
    url: Option<String>,
}
#[derive(Deserialize)]
struct SearchBody {
    artist: Option<String>,
    title: Option<String>,
}
#[derive(Deserialize)]
struct DownloadBody {
    url: Option<String>,
    format: Option<String>,
}
#[derive(Serialize)]
struct TapeInfo {
    name: String,
    size: u64,
}

fn handle(mut req: Request, jobs: Jobs, root: &Path, dl_dir: &Path) {
    let method = req.method().clone();
    let url = req.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let resp: Response<Cursor<Vec<u8>>> = if method == Method::Get && (path == "/" || path == "/index.html") {
        serve_file(&root.join("public/index.html"), &req, false)
    } else if method == Method::Get && path == "/api/health" {
        json_resp(&serde_json::json!({
            "ok": true,
            "ytDlp": tool_version("yt-dlp"),
            "ffmpeg": tool_version("ffmpeg"),
        }))
    } else if method == Method::Post && path == "/api/info" {
        let body: Result<UrlBody, _> = json_body(&read_body(&mut req));
        match body.ok().and_then(|b| b.url) {
            Some(raw) => match validate_url(&raw) {
                Ok(u) => match yt_info(&u) {
                    Ok((title, duration, id)) => json_resp(&serde_json::json!({
                        "ok": true, "title": title, "duration": duration, "id": id
                    })),
                    Err(e) => json_resp(&serde_json::json!({ "ok": false, "error": e })),
                },
                Err(e) => err_json(e, 400),
            },
            None => err_json("empty url", 400),
        }
    } else if method == Method::Post && path == "/api/search" {
        let body: Result<SearchBody, _> = json_body(&read_body(&mut req));
        let q = body
            .map(|b| {
                format!(
                    "{} {}",
                    b.artist.unwrap_or_default(),
                    b.title.unwrap_or_default()
                )
            })
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if q.is_empty() {
            err_json("empty query", 400)
        } else if q.len() > 200 {
            err_json("query too long", 400)
        } else {
            let out = Command::new("yt-dlp")
                .args(build_search_args(&q, 5))
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    let results: Vec<_> = parse_search_output(&text)
                        .into_iter()
                        .map(|(id, title)| {
                            serde_json::json!({
                                "id": id,
                                "title": title,
                                "url": format!("https://www.youtube.com/watch?v={id}"),
                            })
                        })
                        .collect();
                    json_resp(&serde_json::json!({ "ok": true, "results": results }))
                }
                _ => json_resp(&serde_json::json!({ "ok": false, "error": "search failed" })),
            }
        }
    } else if method == Method::Post && path == "/api/download" {
        let body: Result<DownloadBody, _> = json_body(&read_body(&mut req));
        let (raw, fmt) = match body {
            Ok(b) => (b.url.unwrap_or_default(), b.format.unwrap_or_else(|| "mp3".into())),
            Err(_) => (String::new(), "mp3".to_string()),
        };
        let fmt = if valid_format(&fmt) { fmt } else { "mp3".to_string() };
        match validate_url(&raw) {
            Err(e) => err_json(e, 400),
            Ok(u) => {
                let (title, duration) = yt_info(&u)
                    .map(|(t, d, _)| (t, d))
                    .unwrap_or_else(|_| ("untitled".into(), "".into()));
                let id = job_id();
                {
                    let mut m = jobs.lock().unwrap();
                    m.insert(
                        id.clone(),
                        Job {
                            id: id.clone(),
                            status: "working".into(),
                            phase: "downloading".into(),
                            progress: 0,
                            title: title.clone(),
                            duration: duration.clone(),
                            filename: String::new(),
                            error: String::new(),
                            child_pid: None,
                        },
                    );
                }
                let jobs2 = jobs.clone();
                let dl = dl_dir.to_path_buf();
                let job_id_out = id.clone();
                thread::spawn(move || run_download(jobs2, id, u, fmt, dl));
                json_resp(&serde_json::json!({
                    "ok": true, "jobId": job_id_out, "title": title, "duration": duration
                }))
            }
        }
    } else if method == Method::Get && path.starts_with("/api/jobs/") && !path.ends_with("/cancel") {
        let id = path.trim_start_matches("/api/jobs/");
        let m = jobs.lock().unwrap();
        match m.get(id) {
            Some(j) => json_resp(&j.view()),
            None => err_json("unknown job", 404),
        }
    } else if method == Method::Post && path.starts_with("/api/jobs/") && path.ends_with("/cancel") {
        let id = path
            .trim_start_matches("/api/jobs/")
            .trim_end_matches("/cancel")
            .trim_matches('/');
        let mut m = jobs.lock().unwrap();
        match m.get_mut(id) {
            Some(j) => match j.child_pid.take() {
                Some(pid) => {
                    // SIGTERM the yt-dlp child; its pipes close and the worker
                    // thread wraps up on its own.
                    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    j.status = "error".into();
                    j.error = "cancelled".into();
                    json_resp(&serde_json::json!({ "ok": true }))
                }
                None => err_json("nothing to cancel", 404),
            },
            None => err_json("nothing to cancel", 404),
        }
    } else if method == Method::Get && path == "/api/tapes" {
        let mut tapes: Vec<(std::time::SystemTime, TapeInfo)> = Vec::new();
        if let Ok(rd) = fs::read_dir(dl_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !safe_name(&name) {
                    continue;
                }
                if let Ok(md) = e.metadata() {
                    tapes.push((
                        md.modified().unwrap_or(std::time::UNIX_EPOCH),
                        TapeInfo { name, size: md.len() },
                    ));
                }
            }
        }
        tapes.sort_by(|a, b| b.0.cmp(&a.0));
        let list: Vec<TapeInfo> = tapes.into_iter().map(|(_, t)| t).collect();
        json_resp(&serde_json::json!({ "ok": true, "tapes": list }))
    } else if (method == Method::Get || method == Method::Delete) && path.starts_with("/api/tapes/") {
        let name = pct_decode(path.trim_start_matches("/api/tapes/"));
        if !safe_name(&name) {
            err_json("nope", 403)
        } else {
            let fp = dl_dir.join(&name);
            // containment check
            let contained = fp
                .canonicalize()
                .map(|c| c.starts_with(dl_dir.canonicalize().unwrap_or_else(|_| dl_dir.to_path_buf())))
                .unwrap_or(false);
            if !contained || !fp.is_file() {
                Response::from_string("not found").with_status_code(StatusCode(404))
            } else if method == Method::Delete {
                let _ = fs::remove_file(&fp);
                json_resp(&serde_json::json!({ "ok": true }))
            } else {
                let dl = query.split('&').any(|kv| kv == "dl=1");
                serve_file(&fp, &req, dl)
            }
        }
    } else if method == Method::Get && !path.starts_with("/api/") {
        // static
        let rel = path.trim_start_matches('/');
        if rel.contains("..") {
            Response::from_string("nope").with_status_code(StatusCode(403))
        } else {
            let fp = root.join("public").join(rel);
            let public = root
                .join("public")
                .canonicalize()
                .unwrap_or_else(|_| root.join("public"));
            // A failed canonicalize means the file doesn't exist (nothing to
            // escape through); ".." was already rejected lexically above.
            let contained = fp.canonicalize().map(|c| c.starts_with(&public)).unwrap_or(true);
            if !contained {
                Response::from_string("nope").with_status_code(StatusCode(403))
            } else {
                serve_file(&fp, &req, false)
            }
        }
    } else {
        err_json("unknown route", 404)
    };

    let _ = req.respond(resp);
}

fn main() {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dl_dir = root.join("downloads");
    fs::create_dir_all(&dl_dir).ok();

    let yv = tool_version("yt-dlp");
    let fv = tool_version("ffmpeg");
    println!("tape-deck-rs on http://127.0.0.1:{PORT}");
    println!(
        "yt-dlp: {} | ffmpeg: {}",
        yv.as_deref().unwrap_or("NOT FOUND"),
        fv.as_deref().unwrap_or("NOT FOUND")
    );

    let jobs: Jobs = Arc::new(Mutex::new(HashMap::new()));
    let server = Server::http(format!("127.0.0.1:{PORT}")).expect("bind 3030");
    for req in server.incoming_requests() {
        let jobs = jobs.clone();
        let root = root.clone();
        let dl_dir = dl_dir.clone();
        thread::spawn(move || handle(req, jobs, &root, &dl_dir));
    }
}
