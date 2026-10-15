//! tape-deck-rs: pure helpers (URL validation, yt-dlp arg building,
//! progress parsing, filename safety, error classification). Fully unit-tested.

use regex::Regex;
use std::sync::OnceLock;

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(https?://)?(www\.|music\.|m\.)?(youtube\.com/(watch\?[^ ]*v=|shorts/|embed/|live/)|youtu\.be/)[\w-]{11}[A-Za-z0-9\-_?=&%.~]*$",
        )
        .unwrap()
    })
}

fn id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\w-]{11}$").unwrap())
}

/// Validate a YouTube URL. Returns the normalized (scheme-prefixed) URL.
pub fn validate_url(raw: &str) -> Result<String, &'static str> {
    let url = raw.trim();
    if url.is_empty() {
        return Err("empty url");
    }
    if !url_re().is_match(url) {
        return Err("not a youtube url");
    }
    Ok(if url.starts_with("http") {
        url.to_string()
    } else {
        format!("https://{url}")
    })
}

pub fn valid_format(f: &str) -> bool {
    matches!(f, "mp3" | "m4a" | "flac")
}

/// Build yt-dlp argv for an audio download. out_dir must exist.
pub fn build_args(url: &str, format: &str, out_dir: &str) -> Vec<String> {
    let mut args = vec![
        "--no-playlist".to_string(),
        "--restrict-filenames".to_string(),
        "--newline".to_string(),
        // android player client dodges most of YouTube's 403/bot-check blocks;
        // fine for an audio-only app (audio streams are fully available)
        "--extractor-args".to_string(),
        "youtube:player_client=android".to_string(),
        "--print".to_string(),
        "after_move:filepath".to_string(),
        "-o".to_string(),
        format!("{out_dir}/%(title).60s_[%(id)s].%(ext)s"),
    ];
    match format {
        "mp3" => args.extend(
            ["-x", "--audio-format", "mp3", "--audio-quality", "192K"]
                .iter()
                .map(|s| s.to_string()),
        ),
        "m4a" => args.extend(
            ["-f", "bestaudio[ext=m4a]/bestaudio/best"]
                .iter()
                .map(|s| s.to_string()),
        ),
        _ => args.extend(
            ["-x", "--audio-format", "flac"]
                .iter()
                .map(|s| s.to_string()),
        ),
    }
    args.push(url.to_string());
    args
}

/// Build yt-dlp argv for a YouTube search (flat, fast: id + title only).
pub fn build_search_args(query: &str, n: u32) -> Vec<String> {
    let count = n.clamp(1, 10);
    vec![
        format!("ytsearch{count}:{query}"),
        "--flat-playlist".to_string(),
        "--no-warnings".to_string(),
        "--print".to_string(),
        "%(id)s\t%(title)s".to_string(),
    ]
}

#[derive(Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Converting,
}

/// Parse a yt-dlp stderr line. Returns (progress 0-100, phase).
pub fn parse_progress(line: &str) -> Option<(u32, Phase)> {
    if line.contains("[ExtractAudio]") {
        return Some((95, Phase::Converting));
    }
    // [download]  12.5% of ~4.20MiB at 1.20MiB/s ETA 00:03
    let pct = line
        .split("[download]")
        .nth(1)?
        .trim_start()
        .split('%')
        .next()?
        .trim()
        .parse::<f64>()
        .ok()?;
    let pct = pct.clamp(0.0, 100.0);
    // reserve the top 10% for the extract/convert step
    Some(((pct * 0.9).round() as u32, Phase::Downloading))
}

/// Parse tab-separated search output into (id, title) pairs.
pub fn parse_search_output(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Some(tab) = t.find('\t') else { continue };
        let id = t[..tab].trim();
        let title = t[tab + 1..].trim();
        if id_re().is_match(id) && !title.is_empty() {
            out.push((id.to_string(), title.to_string()));
        }
    }
    out
}

/// True if a tape filename is safe to serve from the downloads dir.
pub fn safe_name(name: &str) -> bool {
    if name.is_empty() || name.len() >= 256 {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    let lower = name.to_lowercase();
    ["mp3", "m4a", "flac", "wav", "ogg", "opus", "webm"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

/// "3:45" -> 225 seconds. Handles H:MM:SS too.
#[allow(dead_code)]
pub fn duration_to_sec(s: &str) -> u64 {
    let parts: Vec<u64> = s.trim().split(':').map(|p| p.parse().unwrap_or(u64::MAX)).collect();
    if parts.iter().any(|&p| p == u64::MAX) {
        return 0;
    }
    parts.iter().fold(0, |acc, &p| acc * 60 + p)
}

/// Turn yt-dlp's stderr tail + exit code into something a human can act on.
pub fn friendly_error(stderr_tail: &str, code: i32) -> String {
    let t = stderr_tail.to_lowercase();
    if t.contains("sign in to confirm you're not a bot")
        || t.contains("sign in to confirm you are not a bot")
    {
        return "youtube bot-check — wait a bit and retry, or update yt-dlp (yt-dlp -U)".into();
    }
    if t.contains("http error 403") || t.contains("403: forbidden") {
        return "youtube blocked the request (403) — update yt-dlp (yt-dlp -U) and retry; if it persists, wait a few minutes or try another video".into();
    }
    if t.contains("video unavailable") {
        return "video unavailable (removed / region-blocked)".into();
    }
    if t.contains("private video") {
        return "private video — can't download".into();
    }
    if t.contains("429") || t.contains("too many requests") {
        return "youtube rate-limited us — wait a few minutes and retry".into();
    }
    if t.contains("unsupported url") {
        return "unsupported url".into();
    }
    if t.contains("no video formats found") || t.contains("requested format not available") {
        return "no audio stream found — try updating yt-dlp (yt-dlp -U)".into();
    }
    if (t.contains("nsig") || t.contains("signature")) && t.contains("extract") {
        return "youtube changed something — update yt-dlp (yt-dlp -U)".into();
    }
    let last = stderr_tail
        .lines()
        .map(|l| {
            let l = l.trim();
            l.strip_prefix("ERROR:")
                .or_else(|| l.strip_prefix("WARNING:"))
                .unwrap_or(l)
                .trim()
        })
        .filter(|l| !l.is_empty())
        .last();
    match last {
        Some(l) => l.chars().take(140).collect(),
        None => format!("yt-dlp exited with code {code}"),
    }
}

/// Percent-decode a URL path segment.
pub fn pct_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4 | l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_accepts_watch() {
        assert!(validate_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").is_ok());
    }
    #[test]
    fn url_accepts_short() {
        assert!(validate_url("https://youtu.be/dQw4w9WgXcQ").is_ok());
    }
    #[test]
    fn url_accepts_shorts() {
        assert!(validate_url("https://www.youtube.com/shorts/dQw4w9WgXcQ").is_ok());
    }
    #[test]
    fn url_accepts_music() {
        assert!(validate_url("https://music.youtube.com/watch?v=dQw4w9WgXcQ").is_ok());
    }
    #[test]
    fn url_rejects_non_youtube() {
        assert!(validate_url("https://vimeo.com/12345").is_err());
    }
    #[test]
    fn url_rejects_empty() {
        assert!(validate_url("").is_err());
    }
    #[test]
    fn url_rejects_injection() {
        assert!(validate_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ; rm -rf /").is_err());
    }

    #[test]
    fn args_mp3_extracts() {
        let a = build_args("https://youtu.be/abc123def45", "mp3", "/tmp/dl");
        assert!(a.contains(&"--audio-format".to_string()));
        assert!(a.contains(&"mp3".to_string()));
        assert!(a.contains(&"--no-playlist".to_string()));
        assert_eq!(a.last().unwrap(), "https://youtu.be/abc123def45");
    }
    #[test]
    fn args_m4a_native() {
        let a = build_args("https://youtu.be/abc123def45", "m4a", "/tmp/dl");
        assert!(a.iter().any(|s| s.contains("bestaudio[ext=m4a]")));
        assert!(!a.contains(&"-x".to_string()));
    }
    #[test]
    fn args_flac() {
        let a = build_args("https://youtu.be/abc123def45", "flac", "/tmp/dl");
        assert!(a.contains(&"flac".to_string()));
    }
    #[test]
    fn args_android_client() {
        let a = build_args("https://youtu.be/abc123def45", "mp3", "/tmp/dl");
        let i = a.iter().position(|s| s == "--extractor-args").unwrap();
        assert_eq!(a[i + 1], "youtube:player_client=android");
    }

    #[test]
    fn search_args_shape() {
        let a = build_search_args("rick astley", 5);
        assert_eq!(a[0], "ytsearch5:rick astley");
        assert!(a.contains(&"--flat-playlist".to_string()));
        assert!(a.contains(&"%(id)s\t%(title)s".to_string()));
    }
    #[test]
    fn search_args_clamped() {
        assert!(build_search_args("x", 99)[0].starts_with("ytsearch10:"));
        assert!(build_search_args("x", 0)[0].starts_with("ytsearch1:"));
    }

    #[test]
    fn progress_parses_percent() {
        assert_eq!(
            parse_progress("[download]  50.0% of ~4.20MiB at 1.20MiB/s ETA 00:03"),
            Some((45, Phase::Downloading))
        );
    }
    #[test]
    fn progress_detects_extract() {
        assert_eq!(
            parse_progress("[ExtractAudio] Destination: song.mp3"),
            Some((95, Phase::Converting))
        );
    }
    #[test]
    fn progress_ignores_other() {
        assert_eq!(parse_progress("[youtube] Extracting video information"), None);
        assert_eq!(parse_progress(""), None);
    }

    #[test]
    fn search_output_parses() {
        let out = parse_search_output(
            "dQw4w9WgXcQ\tRick Astley - Never Gonna Give You Up\nabc123def45\tSome Other Song\n\ngarbage-line\n",
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "dQw4w9WgXcQ");
        assert_eq!(out[0].1, "Rick Astley - Never Gonna Give You Up");
    }
    #[test]
    fn search_output_rejects_bad_ids() {
        assert!(parse_search_output("short\tTitle").is_empty());
    }

    #[test]
    fn name_accepts_audio() {
        assert!(safe_name("song_[abc123].mp3"));
    }
    #[test]
    fn name_rejects_traversal() {
        assert!(!safe_name("../secret"));
        assert!(!safe_name("a/b.mp3"));
    }
    #[test]
    fn name_rejects_non_audio() {
        assert!(!safe_name("evil.sh"));
    }

    #[test]
    fn duration_parses() {
        assert_eq!(duration_to_sec("3:45"), 225);
        assert_eq!(duration_to_sec("1:02:03"), 3723);
        assert_eq!(duration_to_sec("live"), 0);
    }

    #[test]
    fn err_bot_check() {
        assert!(friendly_error("ERROR: Sign in to confirm you're not a bot", 1).contains("bot-check"));
    }
    #[test]
    fn err_403() {
        assert!(
            friendly_error("ERROR: Unable to download video data: HTTP Error 403: Forbidden", 1)
                .contains("403")
        );
    }
    #[test]
    fn err_unavailable() {
        assert!(friendly_error("ERROR: Video unavailable", 1).contains("unavailable"));
    }
    #[test]
    fn err_rate_limit() {
        assert!(friendly_error("ERROR: HTTP Error 429: Too Many Requests", 1).contains("rate-limited"));
    }
    #[test]
    fn err_falls_back_to_line() {
        assert_eq!(
            friendly_error("WARNING: blah\nERROR: something broke badly", 1),
            "something broke badly"
        );
    }
    #[test]
    fn err_falls_back_to_code() {
        assert_eq!(friendly_error("", 1), "yt-dlp exited with code 1");
    }

    #[test]
    fn decode_pct() {
        assert_eq!(pct_decode("hello%20world.mp3"), "hello world.mp3");
        assert_eq!(pct_decode("a%2Fb"), "a/b");
        assert_eq!(pct_decode("100%"), "100%");
    }
}
