<p align="center">
  <img src="docs/logo.png" width="112" alt="RDM">
</p>

<h1 align="center">RDM — Rust Download Manager</h1>

<p align="center">
  A fast, good-looking, open-source download manager for <b>Windows 11</b> and <b>Linux</b>.<br>
  Up to 64 connections per file, browser extension, videos and YouTube, built-in VirusTotal scans.
</p>

<p align="center">
  <a href="https://github.com/vincentxjoubert-lang/rdm/releases/latest"><img src="https://img.shields.io/github/v/release/vincentxjoubert-lang/rdm?label=version&color=6a5cff" alt="Version"></a>
  <a href="https://github.com/vincentxjoubert-lang/rdm/releases"><img src="https://img.shields.io/github/downloads/vincentxjoubert-lang/rdm/total?color=3d8bff" alt="Downloads"></a>
  <a href="https://github.com/vincentxjoubert-lang/rdm/actions"><img src="https://img.shields.io/github/actions/workflow/status/vincentxjoubert-lang/rdm/release.yml?label=build" alt="Build"></a>
  <img src="https://img.shields.io/badge/Rust-2024-b24cff" alt="Rust">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-34d399" alt="MIT"></a>
</p>

<p align="center">
  <img src="docs/screenshots/dashboard-dark.png" width="880" alt="RDM, dark theme">
</p>

RDM speaks **16 languages** (English, French, Spanish, German, Italian, Portuguese, Dutch, Polish, Russian, Ukrainian, Turkish, Vietnamese, Indonesian, Chinese (Simplified), Japanese, Korean): it follows the system's language, and the **Settings → Language** drop-down switches at once. Translations live in `crates/app/locales/<code>.json` (English text → translation; a missing text shows in English). Right-to-left and complex scripts (Arabic, Hebrew, Hindi…) are not supported yet: the UI toolkit cannot shape or reorder them. The browser extension follows the browser's language.

## Download

| System | File | Installation |
|---|---|---|
| **Windows 10 / 11** | [`RDM-x.y.z-x64-setup.msi`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | Double-click. Installed for your account (no administrator rights), Start menu and **Desktop**. A running RDM is closed cleanly and started again. Later updates install silently from RDM. |
| **Ubuntu 26.04** | source code | `sh install-ubuntu.sh` (see [Installation](#installation)): icon in the GNOME applications, starts at login. |
| **Debian / Ubuntu** | [`rdm_*.deb`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | `sudo apt install ./rdm_*.deb` |
| **Fedora / openSUSE** | [`rdm-*.rpm`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | `sudo dnf install ./rdm-*.rpm` |
| **Linux (others)** | [`rdm-linux-x64.tar.gz`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | Extract, then `sh install.sh` |
| **Chrome / Brave / Opera / Edge** | from RDM, or [`rdm-chromium.zip`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | See [Browser extension](#browser-extension) |
| **Firefox / Waterfox** | from RDM, or [`rdm-firefox.xpi`](https://github.com/vincentxjoubert-lang/rdm/releases/latest) | See [Browser extension](#browser-extension) |

## Screenshots

| Light theme | VirusTotal report |
|---|---|
| <img src="docs/screenshots/light-theme.png" alt="Light theme"> | <img src="docs/screenshots/virustotal-report.png" alt="VirusTotal report"> |
| **VirusTotal badges on completed files** | **Settings** |
| <img src="docs/screenshots/virustotal-badges.png" alt="VirusTotal badges"> | <img src="docs/screenshots/settings.png" alt="Settings"> |


## Features

- **Engine**: up to 64 connections per file (IDM: 32), dynamic segmentation (a free connection takes over part of the piece expected to finish last, split by measured speed so that both parts end together), pause/resume. The first request already brings the first bytes of the file (one round trip less), and all connections write into the file through one handle, each at its own offset.
- **Adaptive connections**: a download starts with 8 connections and doubles them every half second while that makes it faster (a hill climb on the measured throughput); when connections fail (timeouts, cuts) it drops some and the others take over their share. A weak Wi-Fi, a saturated router or a picky server find their pace instead of failing with "timed out". One DNS lookup per host is shared by all connections, and write buffers follow the number of connections, so a slow disk or a small machine is not flooded.
- **Picky servers**: a server refusing connections (429/503) is asked for no more than it accepted, its `Retry-After` is obeyed, and both are remembered for the next downloads from it (30 minutes). A server counting *requests* (some mirrors) gets as few as possible: one request runs on through the free pieces of the file. A server that cuts off browser-like programs is asked again as a common download tool.
- **Stragglers**: a connection far slower than the others from its start (a bad route, a busy server behind the load balancer), or silent while the others receive, is closed and replaced at once; its piece goes to the others — the end of a download no longer waits for one slow connection.
- **Queues**: the main queue plus **named queues**, each with its own number of simultaneous downloads (right-click a download → *Move to*).
- **Speed limits**: a global one, and one **per download** (right-click → *Limit the speed…*), applied at once.
- **Change the link** of a download (right-click → *Change the link…*) without losing what is already downloaded: for an expired or moved link. RDM checks that the new link serves the same file (same size) before using it.
- **If the file already exists**: ask (the default: RDM comes to the front, keep both or replace — for browser downloads and pasted or copied links alike), rename (`name (1).ext`), overwrite, or skip — your choice in the settings.
- **Checksums**: paste the MD5, SHA-1, SHA-256 or SHA-512 published by a site (right-click → *Verify a checksum…*): RDM checks the file as soon as it is complete and tells you if it does not match.
- **Proxies**: none, the system's, or your own (HTTP, HTTPS, SOCKS5, SOCKS4a; with `socks5h://` names are resolved by the proxy), with a **Test** button — and an **automatic mode**: direct first, and a download that is blocked or very slow switches to the proxy by itself. A proxy address RDM cannot use is an error: RDM never goes around it.
- **Site logins** (HTTP Basic), sent only to their site over HTTPS; passwords encrypted with Windows' DPAPI (or kept in a file only you can read on Linux). A site with a broken certificate can be accepted **for one download only** (right-click → *Accept an invalid certificate*).
- **Clipboard**: copy a link to a captured format anywhere and RDM offers to download it (or downloads it right away, or ignores it — your choice). What a password manager copies (it marks it private) is not even read, on Windows.
- **Network outages**: the last connection of a download keeps trying for up to 3 minutes without data (Wi-Fi dropping, sleep, server briefly gone); beyond that, RDM puts the download back in the queue and **retries by itself** (after 5 s, 10 s, 20 s… then every 2 minutes, for about half an hour), from where it stopped. The card shows the reason and the countdown, with a *Retry now* button. A server that cannot resume starts the file over (5 times at most).
- **Instant add**: a link sent by the browser appears in the list at once; the real file name is asked from the server in the background (10 s at most). The same link received twice within 5 s is added once.
- **Restart**: downloads running when RDM closed (or the computer shut down) resume by themselves at the next start.
- **Speed**: sparse files on Windows (otherwise NTFS fills the file with zeros before each distant segment: the disk wrote everything twice and connections waited); no Windows 11 "EcoQoS" throttling when RDM is in the background; network and disk threads above normal priority on Windows.
- **Crash-proof**: a resume point is saved every 20 s while downloading (data synced first). A crash, a shutdown or a logout only loses the last seconds; on Linux, logging out (SIGTERM) stops RDM cleanly. A download resumed later (after a restart, days afterwards) continues only if the file on the server is still the same version (same size, same `Last-Modified` or `ETag`); otherwise it starts over rather than mixing two versions.
- **Categories**: Videos, Music, Archives, Programs, Documents, Other. One folder per category, **changeable** in the settings.
- **Captured formats**: more than 150 extensions, list editable in the settings, split archives (`.r00`, `.001`…) always included.
- **HLS (`.m3u8`)**: quality choice, **separate audio tracks** merged automatically, `EXT-X-BYTERANGE`, AES-128.
- **YouTube**: "video + audio" qualities merged into one `.mp4` without re-encoding, audio only, HLS streams.
- **Interface**: dashboard with the live speed and last minute's chart, a card per download (colour per category, animated progress bars), Inter font and Phosphor icons, light and dark themes (one click), discreet in-window notifications. Search (**Ctrl+F**), filters, time left; right-click to open, show in folder, copy the link or the path, **SHA-256**, delete (after confirming); double-click to open; **Ctrl+V anywhere**. The window reopens at its size, position and maximized state.
- **VirusTotal**: *Scan with VirusTotal* on completed files up to 650 MB (VirusTotal refuses larger files: no button). See below.
- **Notification area**: RDM stays there when the window is closed (Open, Resume all, Pause all, Quit); starting RDM again shows the window. The window is really closed (it leaves the taskbar and frees its graphics memory) and *Open* creates a new one, at the same size (and place, except on Wayland, which does not allow it). In the notification area RDM uses virtually no CPU. *Quit* closes RDM in every situation, even with the window minimized or stuck.
- **Notification** when a download completes; **start with the system** (minimized).
- Portable mode: `RDM_CONFIG_DIR=<folder>`.

Measured on a 100 MB file (integrity checked by SHA-256 against `curl`):

| | Throughput |
|---|---|
| `curl`, 1 connection | 1.8 MB/s |
| RDM, 32 connections | 49.4 MB/s |
| RDM, 64 connections | 83.9 MB/s |

## Updates

RDM looks for a new version on this repository at start, then once a day: a single request to GitHub's API, nothing else (can be turned off in **Settings → Updates**, where *Check now* also checks on demand). When a version is out, a card appears in the sidebar; one click installs it, **silently, from RDM itself** — no web page, no installer window. With **Install automatically** on, RDM does it by itself as soon as nothing is downloading.

1. RDM downloads the package from GitHub (with retries if the connection drops), then **checks it**: size and SHA-256 published by GitHub, the file's own format, and an **Ed25519 signature** made by the release workflow with a key only it holds. A tampered or substituted package — even through a compromised GitHub account — is refused.
2. It installs it:
   - **Windows**: RDM closes and an assistant (a copy of RDM, no PowerShell) waits until it is really gone, installs without any window (no administrator prompt; log `rdm-update-<version>.log` in the temporary folder) and starts RDM again. If the installation fails, the assistant says why, offers to try again and starts the current version: RDM is never left closed without explanation.
   - **Linux**, RDM installed in your folders (`install.sh`, the tarball): the binary is replaced in place and RDM restarts.
   - **Linux**, `.deb` / `.rpm`: the package manager installs it after the system's password prompt, and RDM restarts.

Running downloads are paused cleanly and resume afterwards. If the download of the package fails, the card stays there to try again.

> **From RDM 0.1.0 or 0.2.0**: their automatic installation was broken (RDM closed without installing anything). They open the new version's page instead: download `RDM-x.y.z-x64-setup.msi` and double-click it **once** — the installer closes RDM itself. RDM 0.2.1 installs 0.3.0 by itself; signed, silent updates start with 0.3.0.

## Architecture (DDD, layers)

| Crate / folder   | Layer            | Role                                                                       |
|------------------|------------------|----------------------------------------------------------------------------|
| `crates/domain`  | Domain           | `Download` aggregate (states, transitions), `Segment`, categories and formats |
| `crates/engine`  | Infrastructure   | Multi-connection HTTP engine, HLS, MP4 muxing, anti-SSRF, rate limiter, adaptive pacing |
| `crates/app`     | Application + UI | Queues, settings, local bridge `127.0.0.1:9614`, native messaging connector, egui interface, notification area, updates |
| `extension/`     | Browser          | Chrome/Brave/Opera/Edge/Firefox MV3: interception, video detection, YouTube extractor (bundled in RDM, which installs it) |

Where to look, one concern per file:

| Where | What |
|---|---|
| `engine/src/transfer.rs` · `slots.rs` · `pace.rs` | One download: its connections, the pieces they share (dynamic segmentation, stragglers), how many connections the server and the network take |
| `engine/src/probe.rs` · `net.rs` · `rate.rs` | First request (size, name, range support), anti-SSRF and DNS cache, speed limits |
| `engine/src/hls.rs` · `merged.rs` · `mux.rs` | HLS playlists, separate video and audio tracks, their MP4 merge |
| `app/src/manager/` | The application service: `mod.rs` (queues, scheduling, ticker), `add.rs` (new downloads), `entry.rs` (a download of the list), `files.rs` (names on disk), `routes.rs` (proxy, clients, logins), `online.rs` (updates, VirusTotal), `browsers.rs`, `recording.rs`, `checksum.rs`, `clipboard.rs` |
| `app/src/bridge.rs` · `local.rs` · `native.rs` | The local bridge and who may use it, the browsers' native connector |
| `app/src/settings.rs` · `secrets.rs` · `i18n.rs` | Settings files, encrypted passwords, the 16 languages |
| `app/src/update/` | Updates: `mod.rs` (the check, how RDM was installed), `package.rs` (download, SHA-256, Ed25519 signature), `install.rs` (Windows assistant, Linux binary or package) |
| `app/src/virustotal.rs` · `ytdlp.rs` · `extension.rs` | VirusTotal, the YouTube module, the bundled extension |
| `app/src/ui/` | The egui interface: `theme` → `widgets` → `chrome` (sidebar, header, dashboard), `card` (the list), `dialogs/`, `toast` |

## VirusTotal

The shield of a completed file (or its right-click menu) starts the analysis, entirely inside RDM: the website is never opened.

1. RDM computes the file's SHA-256 and looks it up at VirusTotal. **If the file is already known, nothing is uploaded**: the verdict shows within seconds.
2. Otherwise the file is uploaded (up to 650 MB; above 32 MB through the dedicated upload address), with the progress in the card, then RDM waits for the verdict (a few minutes usually).
3. The result shows on the card ("0/72" in green, or the number of engines flagging the file, in red); a click opens the report: verdict breakdown, engines that detect something, fingerprint. A desktop notification also announces the verdict, which is kept from one session to the next.

You need **a free API key** (once): create an account on virustotal.com, copy the key from its "API key" page, paste it in **Settings → Security → VirusTotal** (it is kept encrypted with your passwords). The first click on a shield without a key opens that setting directly. The free API allows 4 requests per minute: RDM runs analyses one at a time and waits if the quota is reached.

Privacy: an uploaded file is shared with antivirus vendors. Do not analyse your personal documents. The file's name is not sent (only its extension).

## Browser extension

**The extension is bundled in RDM, which installs it.** The **Browser extension** card in the sidebar (or **Settings → Browser → Install the extension…**; the window also opens by itself at first start) lists the browsers on this computer — any Chromium- or Firefox-based browser Windows lists as installed (Chrome, Edge, Brave, Opera, Vivaldi, Firefox, Waterfox, LibreWolf, Floorp, Zen…), plus **Add a browser…** for a portable one — and those the extension talks to RDM from ("Connected"). **Remove the extension** makes the extension uninstall itself at its next check-in. **Install** writes the extension to a fixed folder (`%LOCALAPPDATA%\RDM\Extension` on Windows, `~/RDM/Extension` on Linux, readable by Snap and Flatpak browsers), opens the browser on its extensions page and shows the two remaining clicks, with the path to copy. RDM keeps that folder up to date: the extension follows new versions when the browser restarts.

**How the extension reaches RDM**: through RDM's **native messaging connector**, which RDM registers with every browser at start. The browser itself starts the connector, and only for the RDM extension, so no approval is needed — including on Firefox and its derivatives — and **a link sent from a page starts RDM if it is not running**. Browsers that cannot start it (a sandboxed Flatpak or Snap browser) fall back to the local bridge on `127.0.0.1:9614`.

**Chrome, Brave, Opera, Edge, Chromium** (Chromium 121 or later): one and the same extension. On the extensions page → **Developer mode** → **Load unpacked** → the folder RDM shows. Without RDM: `rdm-chromium.zip` from the releases page (or `sh packaging/chromium/build.sh`), extract, then load it the same way. A `.crx` would not help: Chrome, Brave and Edge only install `.crx` files from their store. The extension's ID is fixed (`cgailhenfaoohkakpdacohcnmppepjjl`) and RDM only accepts that one.

**Firefox** (140 or later) only keeps extensions **signed by Mozilla**.

- **Firefox and its derivatives**: **Install** opens the extension's page in the [Firefox store](https://addons.mozilla.org/firefox/addon/rdm-rust-download-manager/) → **Add to Firefox** → **Add**: installed for good, kept up to date by the browser. If the browser cannot be opened on it, or with **Store not opening? Install without the store**, RDM falls back to the package it ships (below).

- **Waterfox** (a Firefox derivative, same extension): **Install** opens the `rdm-firefox.xpi` package in Waterfox → **Add**. Waterfox accepts unsigned extensions: it stays installed (tested with Waterfox 6.7). After an RDM update, *Reinstall* brings the extension to its new version.
- If the GitHub release contains a signed `rdm-firefox.xpi` (the CI signs it when the `AMO_JWT_ISSUER` / `AMO_JWT_SECRET` secrets of a free addons.mozilla.org account are set), **Install** downloads it and Firefox offers to **Add** it: for good.
- Otherwise RDM opens `about:debugging`: **Load Temporary Add-on…** → `manifest.json` of the folder shown. A temporary add-on goes away when Firefox closes. Firefox Developer Edition, Nightly, ESR (`xpinstall.signatures.required` set to `false`) and LibreWolf keep the `rdm-firefox.xpi` file RDM writes next to it installed for good.
- Firefox gives each installation a random origin (`moz-extension://…`): the connector pairs it with RDM automatically. Without the connector, **RDM asks you to allow it** → **Allow**; until then the extension's icon shows a **!** badge and a message in the page explains why (✕ postpones the question by 30 minutes; **Deny** rejects it until RDM restarts).

If RDM is closed, the browser downloads normally.

- Downloads in captured formats go to RDM, with their cookies and referrer (never in private browsing). A small file the browser already finished stays with the browser (no double download). If RDM is busy and slow to answer, the extension applies the last known list of formats rather than letting the file through.
- Every hand-off is confirmed by a small message in the page ("✓ Sent to RDM: file.zip"); on failure the message says why and the browser keeps the download.
- The extension checks in with RDM every 5 minutes while the browser is open: RDM's extension window knows which browsers are connected.
- Explicit hand-offs (⬇ button, right-click) use the tab's own cookies: private windows and Firefox containers never mix their cookies with the normal session's.
- Hovering a video, the **⬇** button opens the list of qualities.
- Right-click a link, a video or a sound → **Download with RDM**. The extension's toolbar button shows RDM's window (and starts RDM if needed).

### YouTube

The ⬇ menu offers these methods:

1. **Direct download** (a few seconds): the extension asks YouTube's player API from the page, with the browser's session, through several client profiles. RDM **checks every link, from the first to the last byte**, before showing it: YouTube sometimes serves the start of a file then refuses the rest (403) when its anti-bot token is missing. Only fully downloadable links are offered, with picture and sound merged without re-encoding.
2. **YouTube module** (optional, when YouTube refuses the direct links): installed on request from the menu, once (about 300 MB in the user's own data folder, no administrator rights): [yt-dlp](https://github.com/yt-dlp/yt-dlp), [Deno](https://deno.com) for YouTube's JavaScript challenges and [bgutil](https://github.com/Brainicism/bgutil-ytdlp-pot-provider), which generates YouTube's anti-bot (proof-of-origin) token locally. Nothing runs unchecked: yt-dlp's checksum list must carry the signature of yt-dlp's own key (written in RDM), and Deno, bgutil and its drawing library are exact versions checked against SHA-256 values written in RDM; no package's install script runs. yt-dlp only resolves the links; RDM verifies and downloads them like the others.
3. **Recording** (always available, about half the video's duration): the page reloads in recording mode, the video plays muted at 2× (YouTube's own speed setting), in the best quality, and RDM receives exactly the data YouTube's player receives. No token is forged and no protection is bypassed. Ads are not recorded. A banner shows the progress and can cancel.

Not supported: DRM content (Netflix, Widevine, FairPlay) and merging WebM streams (VP9/Opus). Recording forces the player to MP4 (H.264/AV1 + AAC).

## Security and privacy

- **No telemetry**. The only connection you did not ask for: the update check (one request to GitHub's API a day, can be turned off).
- **Signed updates**: every package is verified (Ed25519 signature, SHA-256, format) before it is installed.
- **Cookies never written to disk** (memory only). `SameSite=Strict` cookies are only sent for a link of the same site as the page, as a browser does. Cookies and site logins stay on their site: a playlist (HLS) or a separate audio track pointing to another site's servers never receives them, and what was given over HTTPS never travels in clear (an `http://` segment gets none).
- **Passwords** (proxy, site logins) and the **VirusTotal API key** are kept apart from the settings, encrypted with DPAPI on Windows (0600 file on Linux), and sent only to their site, over HTTPS (plain HTTP only for a site you saved as `http://…`).
- **Local bridge** `127.0.0.1`: a connection from a program of **another account** of the computer is closed before it is read (RDM checks which account runs the program at the other end: a program, unlike a browser, could forge the extension's identity); only **our** extension is accepted (ID pinned on Chrome, paired by the connector or approved in RDM on Firefox); a local program (the native connector, `rdm --quit`) must present a secret token RDM renews at each start in your private settings folder — another account of the computer, or a program that cannot read your files, is refused; before sending anything the connector checks that the program on the port runs under your account (another account cannot pose as RDM to collect cookies); `Host` check (anti DNS rebinding); a web page cannot even tell that RDM is installed; request bodies capped at 64 KB; http(s) URLs only. The native connector only relays a fixed list of requests (it cannot close RDM).
- **Anti-SSRF**: a redirect or a playlist coming from the Internet cannot target your local network (router, NAS, printer…) — neither by address (IPv6 forms that carry a local IPv4 address included: NAT64, 6to4) nor by a name resolving there (DNS rebinding included). Downloads from your local network keep working.
- **Sanitized file names**: no `../`, no reserved Windows names, no text-direction characters (a name cannot pass `.exe` off as `.pdf`), bounded length — in bytes too, so that long Japanese or Russian names fit Linux file systems.
- **Private settings folder** on Linux (0700): the download list keeps links with their access tokens.
- **Mark-of-the-Web** (Windows): SmartScreen warns before running a downloaded file, without recording the source URL.
- **Integrity**: every partial response is checked (`Content-Range`: where it starts and the file's size); resume states are validated (contiguous segments, no gap) and written atomically; a merged video is on disk before it takes its name; empty or truncated responses are never reported "complete".
- **Robustness**: playlist sizes and segment counts bounded, and the memory video segments take while waiting for their turn; MP4 parser tested against truncated or corrupted input; a damaged settings file is kept aside (`<name>.bad`) instead of being overwritten.
- TLS through rustls (no OpenSSL); dependencies audited with `cargo audit`.

## Build and tests

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
sh extension/test/check.sh        # extension (Node ≥ 20), manifests, translations, Chromium and Firefox packages
```

Build dependencies on Linux: a C compiler and `pkg-config` (`sudo apt install build-essential pkg-config` or `sudo dnf install gcc pkg-config`). No GTK library is needed.

## Installation

**Ubuntu 26.04, in one command** (from the project folder):

```sh
sh install-ubuntu.sh
```

The script installs the build tools and, if needed, Rust (rustup), builds RDM, installs it for your account with its icon in the **GNOME applications** (Super key → "RDM") and on the **Desktop** (when it shows icons), **starts it at every login** (minimized to the notification area) and starts it now. `sudo` is only used for missing system packages. Uninstall: `sh install-ubuntu.sh --uninstall` (settings are kept).

| System | Install | Uninstall |
|---------|-----------|--------------|
| Windows | `powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1` | `… install.ps1 -Uninstall` |
| Ubuntu  | `sh install-ubuntu.sh` | `sh install-ubuntu.sh --uninstall` |
| Linux   | `sh packaging/linux/install.sh [--autostart]` | `sh packaging/linux/install.sh --uninstall` |
| Debian/Ubuntu | `cargo install cargo-deb && cargo deb -p rdm` → `sudo apt install ./target/debian/rdm_*.deb` | `sudo apt remove rdm` |
| Fedora | `cargo install cargo-generate-rpm && cargo build --release && cargo generate-rpm -p crates/app` → `sudo dnf install ./target/generate-rpm/rdm-*.rpm` | `sudo dnf remove rdm` |

`install.sh` installs RDM in `~/.local` (no administrator rights). With `sudo` it also installs the **missing** system libraries: Wayland/X11, EGL, xkbcommon, the desktop portal (file dialogs) and, on GNOME, the notification-area extension. It supports apt, dnf, zypper and pacman. If the binary is missing and Rust is installed, it builds RDM. `--no-deps` installs nothing system-wide. Checked on Ubuntu 26.04 (GNOME) and Fedora 44 (KDE Plasma); `install-ubuntu.sh` and its `--uninstall` also on Ubuntu 26.04 under Windows (WSL).

Icons: the two vector sources are in `crates/app/assets/icon/` (`rdm-small.svg`, bolder, is used up to 32 px). `python3 packaging/icons/render.py` regenerates every size: Windows `.ico`, Linux icons (16 to 512 px + SVG), the extension's icons, the window and notification-area images.

The GitHub CI (`.github/workflows/release.yml`) builds, lints, tests (Rust and extension, `web-ext lint` for Firefox), audits dependencies, then produces the Windows and Linux binaries, the `.deb`, the `.rpm`, `rdm-chromium.zip` and `rdm-firefox.xpi` (signed by Mozilla when the AMO keys are set), and signs the update packages (secret `RDM_SIGNING_KEY`) in a separate job that runs nothing but `openssl` and `gh`: no build tool or package from a registry ever runs next to the key. Actions are pinned to commits and tools to exact versions (`web-ext`, which signs the Firefox package, with its whole dependency tree: `packaging/web-ext/package-lock.json`); only that release job may write to the repository.
