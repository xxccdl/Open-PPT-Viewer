<div align="center">

# OpenPPTView

**Open the deck. Start teaching.**

A local PPT / PDF presenter built for the computer at the front of a classroom:
first slide in ~2 s, no stutter on page turns, animations intact, pen works on a touchscreen.
Windows 10 / 11 · fully offline · one installer, nothing else to set up

[![release](https://img.shields.io/github/v/release/xxccdl/Open-PPT-Viewer?label=release&color=2ea043)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![downloads](https://img.shields.io/github/downloads/xxccdl/Open-PPT-Viewer/total?label=downloads&color=1f6feb)](https://github.com/xxccdl/Open-PPT-Viewer/releases)
[![license](https://img.shields.io/github/license/xxccdl/Open-PPT-Viewer?color=blue)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![stack](https://img.shields.io/badge/Tauri%202-Rust-f97316)](https://tauri.app)

[**⬇ Download**](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) ·
[Features](#-features) · [Why it is fast](#-why-it-is-fast) · [Classroom ops](#-classroom-ops) · [中文](README.md)

</div>

---

## 🎯 The problem

A teacher in a classroom has about two minutes: plug in the USB stick, find the deck, open it, present.
Most tools lose those two minutes:

| Common approach | What actually happens in class |
|---|---|
| Online presenters | Need a network, a login, and an upload before the first slide |
| Office viewers | Treat a `.pptx` as a document — animations, triggers and embedded media are gone |
| A hand-written renderer | An endless tail of layout quirks: font cmaps, line-height models, group coordinate spaces, shadow stacking… |

**OpenPPTView takes one bet: let the authoring tool draw the pixels, and parse the semantics ourselves.**

| Question | Answered by |
|---|---|
| What does this slide look like | Per-slide bitmap exported by local WPS / PowerPoint |
| What is clickable (links, action buttons) | Raw OOXML |
| Where is the video, and its bytes | Raw OOXML |
| How does this slide appear step by step | Raw OOXML `p:timing`, exported **step by step** by the office app |
| What do the speaker notes say | Raw OOXML |

The built-in renderer is only a **fallback** — used when no office suite is installed or a slide fails to export —
and it says so in the UI instead of pretending to be pixel-perfect.

---

## ⚡ Why it is fast

Measured on a real **174 MB / 39-slide** deck (release build):

| Action | Time |
|---|---|
| `Presentations.Open` | **1.8 s** |
| Export the first slide | **0.4 s** |
| Export the whole deck to vector PDF (background) | 7.2 s |
| From double-click to first slide on screen | **≈ 2 s** |

Two design decisions make this work:

**① Export one slide at a time, not the whole deck.** The teacher only needs page one,
so the first screen is `Open + one slide`; the rest is queued in the background
(~0.4 s per slide — faster than page turns).

**② Export per animation step.** A flattened page contains everything, including answers
that should not be on screen yet. Before exporting, the app hides what belongs to a later
step — shapes via `Shape.Visible`, paragraphs ("bullets appear one by one") by making the
text fill fully transparent through `TextFrame2`. One image per step, every one drawn by
the office engine.

---

## ✨ Features

**Presenting**

- `.pptx` / `.pptm` / `.ppsx` / `.ppsm` / `.potx` / `.potm` / `.pdf` — drag & drop or double-click
- Legacy `.ppt` converted automatically (~0.9 s, cached, instant on the second open)
- Click to advance, right-click to go back, `Esc` to exit; elapsed timer and page indicator
- Per-element entrance, transitions, triggers, automatic advance

**Teaching**

- Pen / highlighter / eraser / laser pointer, with pressure and palm rejection on touch panels
- Blank screen, spotlight, magnifier, timer
- Hyperlinks and action buttons work; embedded video / audio respects the authored trim and looping
- Thumbnail sidebar, speaker notes, zoom & pan, page jump

**After class**

- Annotations stored beside the deck as `<deck>.oppv-annot.json` — **the original file is never modified**,
  and the notes travel with the deck when it is copied elsewhere
- Native installer UI: choose install path, file associations, desktop shortcut

---

## 🧑‍🏫 Classroom ops

The hard part of a school lab is not the lecture — it is "that machine blue-screened again and nobody can describe it".

- **Crash diagnosis**: on the next boot the app picks up what Windows left behind (bugcheck code,
  kernel dump, driver and OS versions), runs read-only repairs (system files / component store /
  disk scan / driver check — each can be switched off to save time), and uploads a packed report
  to an admin dashboard. How long the analysis card stays on screen is configurable (default 5 s, then
  it shrinks to the corner).
- **Remote assistance**: an admin can see which machines are online, view their screen, run `cmd`
  and click on them. The machine dials out — schools have no public IP and should not open a port for this.
- **School / class isolation**: three levels — platform → school → class. Each computer is authorised
  in *Settings → Class* through the browser; afterwards its reports and remote control are visible
  only to **that class's admins**. A computer that has not signed in lands in "unclassified",
  visible to the platform admin only.

> Every piece can be turned off: disabling remote assistance cuts the heartbeat immediately;
> disabling crash diagnosis turns the app into a plain presenter.

---

## 🧱 Architecture

Ten crates, dependencies flow top-down:

| crate | Responsibility |
|---|---|
| `ppt-core` | On-demand OPC/ZIP reading, `DocumentSource` abstraction, SceneGraph IR |
| `ppt-text` | Font resolution, shaping, CJK line breaking and layout |
| `ppt-format-pptx` | PPTX parsing: OOXML → SceneGraph |
| `ppt-format-pdf` | PDF support: rasterisation and PageSource adapter |
| `ppt-render` | SceneGraph rasteriser (the built-in fallback) |
| `ppt-pipeline` | Two-level cache and priority pre-render scheduling |
| `ppt-convert` | Drives local WPS / Office (per-slide bitmaps, per-step export) |
| `ppt-app` | Tauri app: window, commands, custom protocol, file associations, crash diagnosis, remote assist |
| `ppt-installer-ui` | Installer UI: custom-drawn controls (no WebView2) |
| `ppt-installer` | Installer itself (no NSIS) |

"Pixels" and "semantics" are two independent paths assembled by `Pipeline::with_interaction`:

```text
                        ┌──────────────────┐
   local office suite ──▶│  pixels (bitmaps) │──┐
                        └──────────────────┘  │
                                              ├──▶  Pipeline  ──▶  front-end <canvas>
                        ┌──────────────────┐  │
   deck file ──parse────▶│ semantics (OOXML) │──┘
                        └──────────────────┘
```

The front end is plain HTML / CSS / JS with **no build step** (`crates/ppt-app/web`).
The admin backend is a zero-dependency Node service (`server/crash`).

---

## 🚀 Quick start

1. Grab `OpenPPTView_x.y.z_x64-setup.exe` from [Releases](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) and install it.
2. Double-click a deck (or drag it into the window). `F5` presents, `Esc` exits.
3. On a touchscreen, just write with your finger; laser, blank screen and timer live in the toolbar.
4. To attach this computer to a class: *Settings → Crash diagnosis → Class* → **Sign in** →
   pick a class in the browser → authorise; the browser sends you straight back.

> For best fidelity install WPS or PowerPoint (the app drives `KWPP.Application` / `PowerPoint.Application`;
> Word does not count). Without one it still runs, using the built-in renderer, and says so.

---

## 🛠 Building from source

Windows and a stable Rust toolchain.

```powershell
# run
cargo run -p ppt-app --release

# build the installer (output under target\...\bundle\nsis\)
cargo tauri build --bundles nsis

# tests (ppt-installer is excluded: its tests need admin rights and really write to the install dir)
cargo test --workspace --exclude ppt-installer
```

### Where things live

- **Logs**: `%LOCALAPPDATA%\OpenPPTView\logs\openpptview.log`
  (which engine drew the slide, why an export failed, renders slower than 60 ms)
- **Cache**: `%LOCALAPPDATA%\OpenPPTView\cache\`
  - `wps\v<N>\<deck-fingerprint>\<tier>\` — per-slide PNGs and animation frames
  - `convert\v<N>\` — whole-deck vector PDF
  - switching the export mode bumps `<N>`, so old caches expire naturally

### Debugging rendering

```powershell
# draw one slide and print the node list, animation steps, links and media hit areas
cargo run -p ppt-render --example dump_page -- "deck.pptx" "out.png" 0 2.0

# measure per-slide render latency at each scale tier (look at the worst pages, not the average)
cargo run -p ppt-pipeline --example page_latency -- "deck.pptx"
```

---

## ⚠️ Known limitations

- **Windows only**: per-step export, format conversion, the installer and file associations all depend on it.
- **Emphasis animations are approximated**: colour and scale animations do not change visibility,
  which a static frame cannot express — the built-in renderer approximates them.
- **Legacy `.ppt` needs WPS or PowerPoint installed**; otherwise the app hands the file to the system
  default handler and explains why.
- **Disk cache grows**: a 39-slide animated deck measured ~180 MB. Frames are exported once,
  but exported bitmaps are not evicted automatically yet.
- On complex decks the fallback renderer may differ slightly from the original — which is why it is a
  fallback, and why the UI tells the teacher when it is in use.

---

## 📄 License

MIT — see [LICENSE](LICENSE).

Third-party components (Tauri, Rust, hayro, `image`, `windows`, …) keep their own licenses.

<div align="center">

**If this saved your class two minutes, a ⭐ is welcome.**

</div>
