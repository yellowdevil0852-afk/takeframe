# takeframe

A Windows desktop tool (Tauri 2 + Svelte 5) for extracting frames from video. Batch queue, time-range trimming, four sampling modes (all frames / every N frames / fixed FPS / scene-change), max-side scaling, chroma-key background removal with live preview, sprite-sheet export, and manual per-frame retouching (erase/restore brush).

## Development

Requires [Node.js](https://nodejs.org/) and [Rust](https://www.rust-lang.org/tools/install) (`rustup`).

The ffmpeg/ffprobe sidecar binaries aren't checked into this repo — each is 200MB+, over GitHub's file size limit. Download Windows builds (e.g. from [gyan.dev](https://www.gyan.dev/ffmpeg/builds/)) and place them at:

- `src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe`
- `src-tauri/binaries/ffprobe-x86_64-pc-windows-msvc.exe`

Then:

```bash
npm install
npm run tauri dev
```

## Build

```bash
npm run tauri build
```

Produces an MSI installer and a standalone `.exe` under `src-tauri/target/release/`.
