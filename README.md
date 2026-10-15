# tape-deck-rs

Rust port of [tape-deck](https://github.com/sefuwunder/tape-deck) — a
YouTube audio downloader with a 16-bit soul. The whole thing is a
cassette deck: paste a URL (or search artist/title), press **● REC**,
and watch the reels spool while yt-dlp lays the audio down to "tape."

Same JSON API and same static UI as the Bun original; only the server
is rewritten — `tiny_http` + threads, no async runtime.

## The metaphor

- **● REC** — downloads the audio (this is the download button)
- **Reels + tape counter** — spool and advance with download progress;
  the tape pack visibly shifts from the left reel to the right
- **▶ PLAY** — plays the finished tape through the deck; VU needles dance
  to the real audio via WebAudio, reels spin
- **■ STOP** — stops playback, or cancels a recording in progress
- **◀◀ / ▶▶** — seek ∓10s during playback (HTTP Range supported)
- **EJECT ⤓** — downloads the file to your device
- **Tape type** — TYPE I = MP3 192k, TYPE II = M4A 256k, METAL = FLAC
- **Tape shelf** — your recorded collection; LOAD puts a tape back in the deck

## Run it

Needs a Rust toolchain, plus yt-dlp and ffmpeg on the machine:

```sh
cargo run --release
# → http://127.0.0.1:3030
```

Single binary (`target/release/tape-deck-rs`, ~3.5MB). Run it from the
repo root so it finds `public/` and `downloads/`. Takes over port 3030 —
stop the Bun tape-deck first if it's running.

Single videos only (`--no-playlist` is always passed). Files land in
`downloads/`.

## Layout

- `src/main.rs` — tiny_http server, routes, download-job threads
- `src/helpers.rs` — pure helpers: URL validation, yt-dlp arg building,
  progress parsing, filename safety, error classification (+ unit tests)
- `public/index.html` — the whole deck: pixel CSS, transport logic,
  WebAudio VU meters (unchanged from tape-deck)
- API: `GET /api/health`, `POST /api/info`, `POST /api/search`,
  `POST /api/download`, `GET /api/jobs/:id`,
  `POST /api/jobs/:id/cancel`, `GET /api/tapes`,
  `GET /api/tapes/:name` (`?dl=1`, Range), `DELETE /api/tapes/:name`

## Notes

- Downloads and metadata use the android player client
  (`--extractor-args youtube:player_client=android`), which dodges most
  of YouTube's 403/bot-check blocks. Audio-only app, so no downside.
- Progress comes from yt-dlp's `--newline` output; the top 10% of the
  counter is reserved for the extract/convert step.
- Filenames are `--restrict-filenames`'d and include the video id;
  served files are extension-allowlisted and containment-checked.
- Failures surface yt-dlp's real reason (bot-check, 403, rate limit,
  unavailable, ...) instead of a bare exit code.

## Troubleshooting

- Most "code 1"-style failures are an outdated yt-dlp — YouTube changes
  things often. Update it: `yt-dlp -U` (or reinstall if yours came
  from a distro package).
- `429 / too many requests` means YouTube is throttling that IP —
  wait a few minutes and hit REC again.
