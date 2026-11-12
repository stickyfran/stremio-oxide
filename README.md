# Stremio Oxide

Cliente de escritorio nativo para Stremio en Rust con interfaz declarativa Slint (FemtoVG/OpenGL) y motor de reproduccion multimedia libmpv. Disenado como un unico proceso nativo sin Electron, Node.js ni WebViews.

## Caracteristicas

- Proceso unico en codigo maquina (reemplaza Chromium y procesos Node.js).
- Reproduccion acelerada por GPU mediante libmpv con soporte HDR y shaders (FSR, Anime4K).
- Previsualizacion interactiva de trailers y miniaturas de barra de tiempo en tiempo real.
- Motor de streaming y BitTorrent embebido en Rust (rqbit / DHT en proceso).
- Persistencia local ACID en SQLite (WAL) para catalogo, historial y configuracion.
- Integracion con el sistema operativo: Picture-in-Picture (PiP), controles multimedia (MPRIS/SMTC), bandeja del sistema (SNI/Win32) y gestion de geometria de ventana.

## Plataformas y Arquitecturas

- Linux: x86_64 y ARM64 (aarch64) en binario independiente y paquetes DEB.
- Windows: x86_64 y ARM64 (aarch64) con instaladores Inno Setup y paquetes portables.
- macOS: Apple Silicon (ARM64) e Intel x86_64.

## Requisitos de Compilacion

- Rust 1.80+ (edicion 2021/2024).
- Compilador C/C++, CMake, Ninja, pkg-config.

### Dependencias por Plataforma

- Linux (Debian/Ubuntu):
  `sudo apt install build-essential clang cmake ninja-build pkg-config libssl-dev libclang-dev libgtk-3-dev libayatana-appindicator3-dev libmpv-dev libfontconfig1-dev libfreetype6-dev libxkbcommon-dev libwayland-dev libx11-xcb-dev libgl1-mesa-dev libegl1-mesa-dev`
- Windows:
  Visual Studio Build Tools (C++), LLVM (libclang) y vcpkg para dependencias nativas estaticas.
- macOS:
  `brew install mpv pkg-config cmake ninja`

## Compilacion e Instalacion

Clonar el repositorio con submodulos:

```bash
git clone --recursive https://github.com/stickyfran/stremio-oxide.git
cd stremio-oxide
```

Compilar en Release:

```bash
cargo build --release -p stremio-native
```

Instalar localmente en Linux:

```bash
install -m755 target/release/stremio-native ~/.local/bin/stremio-oxide
```

## Arquitectura

Consulte [ARCHITECTURE.md](ARCHITECTURE.md) para el detalle tecnico exhaustivo sobre el modelo de concurrencia, topologia de crates, pipeline de renderizado y arquitectura de compilacion.

## Known Issues

- **Cold Startup Race Condition (Intermittent Catalog Loading)**: Under specific thread scheduling conditions during cold boot, `stremio-core`'s catalog plan may execute before active profile storage hydration finishes, leaving catalog models with `content: None` and showing persistent skeleton placeholders. Navigating back and forth or switching tabs forces a reload. Work is underway to make the startup lifecycle fully deterministic and eliminate any dependency on explicit re-initialization.

## Roadmap & Upcoming Releases

The next releases focus on refining the native experience, visual consistency, and end-to-end media management:

### 1. Robust Lifecycle & Deterministic Startup
- **Zero-Friction Boot Pipeline**: Completely eliminate startup race conditions by decoupling UI readiness from network and database initialization.
- **Permanent State Self-Healing**: Guarantee that catalogs, library sync, and addons hydrate reliably without requiring user intervention, tab switching, or app restarts.

### 2. Search & Filter Bar UI Alignment
- **Cohesive Search Bar Geometry**: Harmonize margins, baselines, and paddings between search inputs and filter selectors across Discover and Search pages.
- **Adaptive Layout**: Ensure search filter dropdowns and active token tags align seamlessly across varying window widths and DPI scaling factors.

### 3. High-Performance Fullscreen Trailer Experience
- **Optimized Trailer Startup**: Fast-path URL resolution and player pre-warming to drastically decrease time-to-first-frame for video trailers.
- **Integrated Fullscreen Trailer Overlay**: Redesign the Details view to embed trailers into an immersive backdrop and full-viewport presentation mode with instant playback controls.
- **Spatial Utilization**: Enhance screen real estate usage in movie and series detail views so promotional media and synopses feel dynamic rather than constrained to small side panels.

### 4. End-to-End Download Manager & Offline Library
- **Already in place**:
  - Interactive source picker before enqueueing a download (direct HTTP, torrent via the local streaming server, or Debrid resolution).
  - Movies, single episodes, complete seasons (`scope = season`) and complete series (`scope = series`) queued in one action, including multi-file torrent packs split per episode.
  - Automatic subtitle downloads next to every video, using the selected stream subtitles plus OpenSubtitles, matching the user's preferred language, the app subtitle setting and the OS locale.
  - Real-time progress bars, download speed, ETA, pause/resume/delete, storage quota checks and background execution with completion notifications.
- **Still missing for a true offline library**:
  - **Subtitle management per download**: choose target languages explicitly in the download dialog, download multiple languages, and re-fetch subtitles later from the Downloads view.
  - **Offline playback wiring**: guarantee the player prefers the local subtitle sidecars (and embedded tracks) when the video is opened from the local library, with no network dependency.
  - **Full-series orchestration as a first-class flow**: resolve every missing episode stream before enqueueing, show aggregate progress for the whole series, and retry failed episodes individually.
  - **Season/series pack verification**: detect when a torrent pack is missing an episode (or contains extras) and surface it instead of silently skipping files.
  - **Integrity and repair**: post-download hash verification, resume after app restart, and automatic re-download of corrupted `.part` files.
  - **Disk lifecycle controls**: per-title storage usage, configurable retention, and safe bulk cleanup of watched content.

### 5. Video Stack Refinements & Future Roadmap
- **Fase 0 — Telemetría y Línea Base de Rendimiento**:
  - Instrumentar métricas continuas del pipeline de video (`frame-drop-count`, `decoder-frame-drop-count`, `vo-delayed-frame-count`, `avsync`, `estimated-vf-fps`, `display-fps`).
  - Benchmarks automatizados de consumo de CPU/GPU en perfiles de prueba estándar (1080p SDR, 4K HEVC 10-bit y 4K HDR10).
- **Fase 4.5 — libmpv 0.42 (completado)**:
  - Runtime Windows fijado a `v0.42.0-38-gbcde6b93c` (API cliente 2.6) en `mpv.lock.json`; Flatpak actualizado a `v0.42.0` con libplacebo `v7.387.0`.
  - El piso de compatibilidad se mantiene en la API 2.5 para que sistemas con libmpv 0.41 sigan funcionando; no se usan entry points exclusivos de 2.6.
  - Nuevo stack de red basado en libcurl con HTTP/2, HTTP/3 y pool de conexiones persistentes.
  - `--tls-verify` pasa a estar activado por defecto en mpv 0.42; se mantiene la verificación TLS con proveedores debrid y addons.
- **Fase 4.6 — Zero-copy VA-API en Wayland (completado)**:
  - Corregido `MPV_RENDER_PARAM_WL_DISPLAY` (era `15`, que corresponde a `DRM_DRAW_SURFACE_SIZE`; el valor correcto es `9`).
  - El contexto principal pasa el `wl_display` real de la ventana, por lo que `hwdec_vaapi` puede usar `vaGetDisplayWl()` y ofrecer un decodificador `vaapi` sin copia.
  - El panel de Estadísticas del reproductor muestra `hwdec-current`, de modo que se distingue `vaapi`, `vaapi-copy` y decodificación por software.
  - El preview de trailers ahora renderiza en el contexto OpenGL de Slint con una textura prestada, igual que el player principal: se eliminaron `glReadPixels` y la copia a CPU por frame. El worker solo decodifica y controla la reproducción.
  - El tamaño del preview se adapta a la superficie activa (tarjeta, panel, ficha o pantalla completa) y el formato de trailer prefiere 1080p.
- **Fase 6 — Evaluación Continua de `gpu-next` en libmpv**:
  - En mpv 0.42 la render API de libmpv sigue exponiendo solo los backends `gpu` y `sw`; `gpu-next` todavía no se puede usar con `vo=libmpv`.
  - El cliente ya prueba `MPV_RENDER_API_TYPE_OPENGL_NEXT` (`opengl-next`) y cae a `opengl` si el runtime no lo soporta; se puede forzar con `STREMIO_MPV_RENDER_BACKEND=gpu|gpu-next`. Queda pendiente construir un libmpv con el parche upstream (PR #18607) para activarlo.
  - Monitorear las cabeceras de libmpv (`render.h`) para un futuro selector de backend (`MPV_RENDER_PARAM_BACKEND`).
  - Preservar `vo=libmpv` con tone mapping automático a SDR hasta que exista soporte de renderizado directo / subsuperficies nativas con composición HDR de 10/16 bits en Slint.

### 6. Keyboard & Gamepad Navigation
- **Already in place**:
  - `Tab` / `Shift+Tab` walk the focus chain of interactive components and move the visible focus ring.
  - `Enter` activates the focused `ButtonSurface` / `FocusScope` (cards, playback controls, dialogs); `Esc` closes overlays and restores focus to the global shortcut scope.
  - Native global and player hotkeys are dispatched in `shortcuts.rs` (playback, seek, volume, subtitles, speed, fullscreen, next episode).
  - Settings toggle and help modal for gamepad controls already exist in the UI.
- **Still missing**:
  - **Spatial (D-pad) navigation**: arrow keys currently do not move focus geometrically between cards or controls, so a 10-foot layout is not yet usable without a pointer.
  - **Real gamepad backend**: no `gilrs` polling or event mapping exists yet; the toggle and dialog are UI-only. Plan: normalize axes/buttons to the same intents as keyboard (up/down/left/right/accept/back/menu), add a radial deadzone and repeat timing.
  - **Focus restoration**: keep track of the last focused element per page/surface so returning from details, player or a modal lands on a predictable control.
  - **Accessibility**: announce focused items and on-screen keyboard support for search input.

## Agradecimientos y Componentes de Terceros

Este proyecto integra y se basa en los siguientes desarrollos de la comunidad de codigo abierto:

- Stremio (SmartCode Ltd): Especificacion del ecosistema, modelos de addons y catalogo.
- stremio-native (perpetus / Matthew Shaji): Base original de la implementacion nativa en Rust y Slint.
- Slint (SixtyFPS GmbH): Framework de interfaz declarativa nativa y renderizado reactivo.
- mpv: Motor de reproduccion multimedia y API libmpv.
- rqbit / librqbit (Igor Izyumin): Implementacion en Rust del motor BitTorrent y streaming DHT.
- ThumbFast (po5): Concepto de busqueda y previsualizacion de fotogramas clave en linea de tiempo.
- Comunidad Rust: Tokio, Rusqlite, Moka, Mimalloc, Winit, Ksni y dependencias del workspace.

## Licencia

Distribuido bajo licencia GPLv3.
