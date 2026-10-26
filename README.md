# Stremio Oxide

Cliente de escritorio nativo para Stremio en Rust con interfaz declarativa Slint (FemtoVG/OpenGL) y motor de reproduccion multimedia libmpv. Disenado como un unico proceso nativo sin Electron, Node.js ni WebViews.

## Caracteristicas

- Proceso unico en codigo maquina (reemplaza Chromium y procesos Node.js).
- Reproduccion acelerada por GPU mediante libmpv con soporte HDR y shaders (FSR, Anime4K).
- Previsualizacion interactiva de trailers y miniaturas de barra de tiempo en tiempo real.
- Motor de streaming y BitTorrent embebido en Rust (rqbit / DHT en proceso).
- Persistencia local ACID en SQLite (WAL) para catalogo, historial y configuracion.
- Integracion con el sistema operativo: Picture-in-Picture (PiP), controles multimedia (MPRIS/SMTC), bandeja del sistema (SNI/Win32) y gestion de geometria de ventana.

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
