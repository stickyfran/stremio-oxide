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

### 4. End-to-End Download Manager & Batch Ingestion
- **Source Selection & Torrent/Debrid Ingestion**: Introduce an interactive stream picker when initiating downloads (direct torrent stream via rqbit or HTTP Debrid source).
- **Single Episode, Movie & Full Season Batching**: Allow users to queue an entire season of a series with one click, automatically resolving episode streams and verifying disk quotas.
- **Actionable Downloads View with Visual Telemetry**:
  - Real-time progress bars, download speed, peer health, and estimated time of completion (ETA).
  - Explicit file management (pause, resume, delete, verify integrity, open containing folder).
  - Native background execution with system notification on download completion.

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
