<div align="center">

# Loadout

A self-hosted download manager for Windows — multi-segment downloads, a browser extension that captures what you click, and a hover-to-download button for YouTube and other `yt-dlp`-supported sites.

![Rust](https://img.shields.io/badge/Rust-1.90%2B-orange?logo=rust)
![Tauri](https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri)
![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)

<!-- TODO: add a screenshot or short GIF of the queue view here, e.g. docs/screenshot.png -->

</div>

## ⚠️ Disclaimer

This is a personal hobby project, provided as-is with no warranty (see [LICENSE](LICENSE)). The hover-to-download feature uses [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) to extract video from sites like YouTube; downloading from those sites generally runs against their Terms of Service. That's a tradeoff you accept for your own use — this project makes no attempt to resist takedowns or enforcement, and you're responsible for how you use it.

## ✨ Features

- **Multi-segment downloads** — splits HTTP(S) downloads across parallel connections for speed, with automatic fallback to a single stream when the server doesn't support ranged requests.
- **Pause, resume, retry, cancel** — plus a per-download and a global default speed limit (bytes/sec).
- **Browser extension handoff** — a Manifest V3 Chrome/Edge extension captures your browser's downloads (with cookies/referer/user-agent) and hands them to the app; falls back to the browser's native download if the app isn't running.
- **Hover-to-download for video sites** — hover any `<video>` on a page to get a download button; picks a resolution via `yt-dlp`, safe against single-page-app navigation (never downloads the wrong video after the page changes under you).
- **Smart link routing** — paste any link into the addbar and it's sniffed and routed automatically: a direct file queues straight away, a page (YouTube, pixeldrain, Google Drive, gofile, ...) goes through `yt-dlp` extraction.
- **Self-managing dependencies** — `yt-dlp` and `ffmpeg` are fetched and verified automatically on first use, with a one-click "Check for update" button for when a site's extractor breaks.
- **Zero-click pairing** — the extension auto-pairs with the app on first use; no setup step required.
- **Toolbar popup** — the extension's toolbar icon shows whether the app is running and the last link you sent it, with a quick link to Options.
- **Runs in the tray** — starts at login, stays out of your way until you need it.

## 🛠️ Tech Stack

| Layer | Technology |
|---|---|
| Desktop app | [Tauri 2](https://tauri.app/) (Rust) |
| Frontend | Plain HTML/CSS/JS — no framework, no build step |
| Local server | [Axum](https://github.com/tokio-rs/axum) (handoff API for the browser extension) |
| Video extraction | [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) + `ffmpeg` (auto-fetched) |
| Browser extension | Manifest V3 (Chrome / Edge) |

## 🚀 Getting Started

### Prerequisites

- **Windows 10/11** (the only platform currently targeted)
- **[Rust](https://rustup.rs/)** 1.90 or newer
- **[Tauri CLI](https://v2.tauri.app/reference/cli/)** for building an installer — `cargo install tauri-cli` (optional for just running in dev mode)
- **Node.js or Deno** on your `PATH` — not required to build the app, but `yt-dlp` needs a JS runtime to solve YouTube's signature challenge; without one, video extraction will fail with only low-res/image formats available

### Installation

```bash
git clone https://github.com/naufalalief/loadout-download-manager.git
cd loadout-download-manager/src-tauri

# Run in development mode
cargo run

# Or build a release installer (.exe) via the Tauri CLI
cargo tauri build
```

The release installer is written to `src-tauri/target/release/bundle/nsis/`.

`yt-dlp` and `ffmpeg` are **not** bundled with the app — they're downloaded automatically into the app's data directory the first time you use the hover-to-download feature.

### Installing the browser extension

The extension is sideloaded (not published to the Chrome Web Store). See [`extension/README.md`](extension/README.md) for step-by-step install instructions, permissions rationale, and how pairing/recovery work.

## 📖 Usage

1. Launch Loadout — it starts in the system tray and opens its window on first run.
2. **Paste a link** into the bar at the top and press Enter to queue it, or just browse normally with the extension installed: downloads you click are captured automatically.
3. Open **Settings** to set the download folder, concurrency limit, and a default speed limit.
4. On a video page (YouTube, etc.), hover the video and click the **Download** button that appears; pick a resolution in the app's queue to start the download.

## 📁 Project Structure

```
loadout-download-manager/
├─ src/                      # Desktop app frontend (plain HTML/CSS/JS, no build step)
│  ├─ index.html
│  ├─ app.js                 # Queue table rendering/polling, Settings, Browser-setup pages
│  └─ styles.css
├─ src-tauri/                # Rust backend
│  ├─ src/
│  │  ├─ download/           # Multi-segment download engine, queue, persistence
│  │  ├─ extract/            # yt-dlp/ffmpeg management and extraction jobs
│  │  ├─ server/             # Local Axum server the browser extension talks to
│  │  ├─ commands.rs         # Tauri commands exposed to the frontend
│  │  ├─ settings.rs
│  │  ├─ tray.rs
│  │  └─ main.rs
│  └─ tests/                 # Integration tests (handoff server, download engine)
└─ extension/                 # Manifest V3 browser extension
   ├─ background.js          # Download capture, pairing, extraction handoff
   ├─ content.js             # Hover-to-download button on video pages
   ├─ popup.html / popup.js  # Toolbar popup (status, last sent, Options link)
   └─ options.js / options.html
```

## 🧪 Running Tests

```bash
cd src-tauri
cargo test
```

## 🗺️ Roadmap

- [ ] Bundle `yt-dlp`/`ffmpeg` with the installer so video extraction works offline on first run
- [ ] macOS support
- [ ] Torrent support (lowest priority — not a near-term goal)

## 🤝 Contributing

This is a personal project, but issues and pull requests are welcome:

1. Fork the repo
2. Create a branch (`git checkout -b feature/your-feature`)
3. Commit your changes
4. Open a pull request

Response times may vary — this is maintained in spare time.

## 📄 License

[MIT](LICENSE) © 2026 Naufal Alief

## 🙏 Acknowledgements

- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) — video extraction
- [`ffmpeg`](https://ffmpeg.org/) (builds via [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds)) — format merging
- [Tauri](https://tauri.app/) — the desktop app shell
