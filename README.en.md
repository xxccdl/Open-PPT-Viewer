<div align="center">

# OpenPPTView

**Open the deck. Start teaching. And when it blue-screens, it goes and looks for you.**

A local PPT / PDF presenter for the classroom computer — plus automatic blue-screen diagnosis, repair, reporting and remote assistance.
Windows 10 / 11 · fully offline · one installer and you are done

[![release](https://img.shields.io/github/v/release/xxccdl/Open-PPT-Viewer?label=release&color=2ea043)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![downloads](https://img.shields.io/github/downloads/xxccdl/Open-PPT-Viewer/total?label=downloads&color=1f6feb)](https://github.com/xxccdl/Open-PPT-Viewer/releases)
[![license](https://img.shields.io/github/license/xxccdl/Open-PPT-Viewer?color=blue)](LICENSE)
[![platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4)](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest)
[![stack](https://img.shields.io/badge/Tauri%202-Rust-f97316)](https://tauri.app)

[**⬇ Download**](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) ·
[Crash diagnosis](#-crash-diagnosis-core) · [Presenting](#-presenting-and-teaching) · [Why it is fast](#-why-it-is-fast) · [中文](README.md)

</div>

---

## 🧯 Crash diagnosis (core)

> The hard part of a school lab is never the lecture. It is "it was fine last period, now it blue-screens at boot and nobody can describe what happened".

Windows actually records everything: the bugcheck code, its four parameters, the kernel dump,
the driver and OS versions at the time. The problem is that nobody ever goes and looks.
**OpenPPTView looks on the next boot — and repairs what can be repaired.**

### What it does

| Stage | What happens | What the teacher has to do |
|---|---|---|
| **Detect** | One check at logon, one more when the app is opened (the two paths are mutually excluded by a file lock, so **one crash is analysed once**) | Nothing |
| **Analyse** | Reads event 1001 and the minidump / full dump; decodes the bugcheck and its parameters; collects OS, patches, CPU, RAM, GPU, machine ID | Glance at the "analysing" card |
| **Check** | Also reads the facts that are usually the real culprit: free space on the system drive (⚠ below 10%), page file, fast startup, memory integrity, Secure Boot, last boot time, **driver load failures** (`7000` / `51`, with the `*.sys` name parsed out) | 1–2 seconds, unnoticeable |
| **Repair** | Read-only passes: `sfc`, DISM component-store scan, `chkdsk /scan`, and disabling non-critical devices Windows already marked as unbootable | Can switch off slow steps, or press "skip this step" |
| **Report** | Packs the code, dump, repair log and check results to the dashboard; if offline it stays local and is resent on the next boot | Nothing |
| **Close the loop** | The dashboard explains the bugcheck, extracts suspicious log lines, and lets you mark it investigating / fixed | The admin |

### Built for people who cannot wait

- **The analysis card stays on screen for a configurable time** — 5 s by default, then it shrinks
  to the corner so it never covers a slide (2 / 5 / 10 / 20 s).
- **Every repair step can be switched off** — a full run takes a dozen minutes; a busy lab keeps
  only the disk scan.
- **"Skip this step"** appears automatically once a step exceeds 20 s — press it and the current
  step ends immediately, the next one starts. Nobody has to kill the process to escape a progress bar.
- **Repair without booting (experimental)** — for boot-related bugchecks (`0x7B` / `0xED` / `0xEF` …)
  the app can arm a custom Windows Recovery environment that repairs offline and then returns to Windows.
- **No double reporting** — the event timestamp and the dump timestamp differ by a few seconds;
  they are treated as one crash, and the server de-duplicates again on arrival.

### Remote assistance + class isolation

- From the dashboard an admin can see which machines are online, **view their screen**, run `cmd`
  and drive mouse/keyboard. The machine dials out — schools have no public IP and should not
  open a port for this.
- Three levels of isolation: **platform → school → class**. Each computer is authorised in
  *Settings → Class* through the browser; afterwards its reports and remote control are visible
  only to **that class's admins**. An unauthorised machine reports to "unclassified",
  visible to the platform admin only.
- One token per computer (only its hash is stored server-side); re-authorising revokes the old
  token. Switch the feature off and the heartbeat stops immediately.

> Crash diagnosis can be turned off entirely — then the app is just a presenter.

---

## 🎯 Why another presenter

A teacher has about two minutes: plug in the USB stick, find the deck, open it, present.
Most tools lose those two minutes:

| Common approach | What actually happens in class |
|---|---|
| Online presenters | Need a network, a login and an upload before the first slide |
| Office viewers | Treat a `.pptx` as a document — animations, triggers and embedded media are gone |
| A hand-written renderer | An endless tail of layout quirks: font cmaps, line-height models, group coordinate spaces… |

**OpenPPTView takes one bet: let the authoring tool draw the pixels, and parse the semantics ourselves.**

| Question | Answered by |
|---|---|
| What does this slide look like | Per-slide bitmap exported by local WPS / PowerPoint |
| What is clickable (links, action buttons) | Raw OOXML |
| Where is the video, and its bytes | Raw OOXML |
| How does this slide appear step by step | Raw OOXML `p:timing`, exported **step by step** by the office app |
| What do the speaker notes say | Raw OOXML |

The built-in renderer is only a **fallback** — used when no office suite is installed or a slide fails to export — and it says so in the UI instead of pretending to be pixel-perfect.

---

## ⚡ Why it is fast

Measured on a real **174 MB / 39-slide** deck (release build):

| Action | Time |
|---|---|
| `Presentations.Open` | **1.8 s** |
| Export the first slide | **0.4 s** |
| Export the whole deck to vector PDF (background) | 7.2 s |
| From double-click to first slide on screen | **≈ 2 s** |

**① Export one slide at a time, not the whole deck.** The teacher only needs page one,
so the first screen is `Open + one slide`; the rest is queued in the background (~0.4 s per slide).

**② Export per animation step.** A flattened page contains everything, including answers
that should not be on screen yet. Before exporting, the app hides what belongs to a later step —
shapes via `Shape.Visible`, paragraphs ("bullets appear one by one") by making the text fill
fully transparent through `TextFrame2`.

---

## ✨ Presenting and teaching

**Presenting**

- `.pptx` / `.pptm` / `.ppsx` / `.ppsm` / `.potx` / `.potm` / `.pdf` — drag & drop or double-click
- Legacy `.ppt` converted automatically (~0.9 s, cached)
- Click to advance, right-click to go back, `Esc` to exit; timer and page indicator
- Per-element entrance, transitions, triggers, automatic advance

**Teaching**

- Pen / highlighter / eraser / laser pointer, with pressure and palm rejection
- Blank screen, spotlight, magnifier, timer
- Hyperlinks and action buttons work; embedded video / audio respects authored trim and looping
- Thumbnail sidebar, speaker notes, zoom & pan, page jump

**After class**

- Annotations stored beside the deck as `<deck>.oppv-annot.json` — **the original file is never modified**
- Native installer UI: install path, file associations, desktop shortcut

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
| `ppt-app` | Tauri app: window, commands, protocol, file associations, **crash diagnosis, remote assist** |
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

The front end is plain HTML / CSS / JS with **no build step**. The admin backend is a
zero-dependency Node service (`server/crash`).

---

## 🚀 Quick start

1. Grab `OpenPPTView_x.y.z_x64-setup.exe` from [Releases](https://github.com/xxccdl/Open-PPT-Viewer/releases/latest) and install it.
2. Double-click a deck (or drag it into the window). `F5` presents, `Esc` exits.
3. On a touchscreen, write with your finger; laser, blank screen and timer live in the toolbar.
4. To attach this computer to a class: *Settings → Crash diagnosis → Class* → **Sign in** →
   pick a class in the browser → authorise; the browser sends you straight back.
5. Do not want crash diagnosis? Switch it off in settings and the logon check exits immediately.

> For best fidelity install WPS or PowerPoint (the app drives `KWPP.Application` / `PowerPoint.Application`;
> Word does not count). Without one it still runs on the built-in renderer, and says so.

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
- **Crash reports**: `%LOCALAPPDATA%\OpenPPTView\crash\` (`reports\`, `outbox\` for unsent packages)
- **Cache**: `%LOCALAPPDATA%\OpenPPTView\cache\`
  - `wps\v<N>\<deck-fingerprint>\<tier>\` — per-slide PNGs and animation frames
  - `convert\v<N>\` — whole-deck vector PDF

### Debugging rendering

```powershell
# draw one slide and print the node list, animation steps, links and media hit areas
cargo run -p ppt-render --example dump_page -- "deck.pptx" "out.png" 0 2.0

# measure per-slide render latency at each scale tier
cargo run -p ppt-pipeline --example page_latency -- "deck.pptx"
```

---

## ⚠️ Known limitations

- **Windows only**: per-step export, format conversion, the installer, file associations and crash diagnosis all depend on it.
- **Repair needs administrator rights**: without them the app analyses and reports, and says why in the log.
- **Emphasis animations are approximated** (colour / scale animations do not change visibility).
- **Legacy `.ppt` needs WPS or PowerPoint installed**; otherwise the file goes to the system default handler.
- **Disk cache grows**: a 39-slide animated deck measured ~180 MB; exported bitmaps are not evicted automatically yet.
- On complex decks the fallback renderer may differ slightly — which is why it is a fallback, and why the UI says so.

---

## 📄 License

MIT — see [LICENSE](LICENSE).

Third-party components (Tauri, Rust, hayro, `image`, `windows`, …) keep their own licenses.

<div align="center">

**If it ever saved you a "why did it blue-screen again", a ⭐ is welcome.**

</div>
