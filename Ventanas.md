# Diagnóstico de Ventana, Modo PiP y Comportamiento de Stremio Oxide

Este documento analiza en detalle el funcionamiento de la ventana principal de Stremio Oxide, su transición al modo Picture-in-Picture (PiP), las causas exactas por las cuales el sistema de ventanas y bordes falló recientemente, y la razón por la que en **Discover** se requería doble clic a diferencia de otras secciones.

---

## 1. Contexto y Arquitectura de la Ventana en Stremio Oxide

### 1.1. Arquitectura de Ventana Única vs Multi-Ventana
* **Stremio Oxide**: Funciona bajo una arquitectura de **ventana única** gestionada por Slint y `winit` sobre el compositor de Wayland (KWin en KDE Plasma 6). Cuando el usuario entra en modo PiP, **no se crea una ventana secundaria**, sino que la misma ventana principal se transforma:
  - Cambia su tamaño a dimensiones reducidas (160×90 hasta 480×270 px).
  - Activa la bandera `player-pip-active` en Slint.
  - Solicita al compositor ponerse en modo "Always on Top" (`WindowLevel::AlwaysOnTop`).
  - Oculta las barras de navegación y el shell del catálogo, dejando visible únicamente el lienzo de reproducción de MPV y los controles compactos OSD.
* **Mitub**: En contraste, Mitub implementa una arquitectura **multi-ventana** (`iced::multi_window`), donde la ventana principal permanece en su sitio y se abre una ventana Wayland complementaria (`Settings { level: AlwaysOnTop, decorations: false, size: ... }`), permitiendo que el gestor de ventanas maneje el drag y el edge-resize de manera aislada con `window::drag` y `window::drag_resize`.

---

## 2. ¿Por qué el programa quedó "siempre al frente" y sin bordes en la ventana normal?

### 2.1. La Causa Raíz: Inyección de Reglas Globales en KWin (`kwinrulesrc`)
Para lograr que la ventana PiP flotara sobre las demás aplicaciones en KDE Wayland y no tuviera bordes nativos feos, se introdujo una regla persistente en `~/.config/kwinrulesrc`:
```ini
[stremio-pip]
above=true
aboverule=3
decorationpolicy=none
decorationpolicyrule=3
description=Stremio Oxide PiP
title=Stremio Oxide
titlematch=2
wmclass=stremio
wmclassmatch=2
```

### 2.2. El Fallo de Coincidencia (Match Failure)
1. **`aboverule=3` y `decorationpolicyrule=3` significan "Force" ("Forzar Siempre")** en KWin.
2. La regla filtraba por `title=Stremio Oxide` con `titlematch=2` (substring match).
3. **El título de la ventana principal de Stremio también es `"Stremio Oxide"`** ([app/ui/app.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint#L177)).
4. En consecuencia, KWin aplicó la regla forzada **a toda la aplicación Stremio en todo momento**, no solo cuando entraba en PiP. La ventana principal perdió sus bordes/decoraciones y quedó permanentemente anclada como "Keep Above" (siempre por delante de las demás ventanas del escritorio).

---

## 3. ¿Por qué el PiP no se podía reposicionar ni redimensionar?

Al entrar en PiP, surgieron dos bloqueos concurrentes:

1. **Conflicto de Reglas KWin (`positionrule=3` y `sizerule=3`)**:
   - Cuando se guardó una posición previa en `kwinrulesrc` con política `3` (Force), KWin ignoraba cualquier intento de mover o redimensionar la ventana por parte del usuario, forzando las coordenadas guardadas.
2. **Superficie de Eventos Táctiles/Ratón en Slint (`pip-drag-touch`)**:
   - En [app/ui/pages/player.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/player.slint), se colocó un `TouchArea` de pantalla completa con `z: 80` que interceptaba los eventos de ratón para llamar a `window.drag_window()` en `PointerEventKind.down`.
   - En Wayland / `winit`, `drag_window()` y `drag_resize_window()` requieren que el evento de clic sea originado de una manera específica compatible con el protocolo `xdg_toplevel.move` / `xdg_toplevel.resize`. En muchos compositores Wayland, si un elemento hijo o un timer en Slint consume o interfiere con la secuencia de eventos, o si la ventana tiene `aboverule=3` de KWin forzada, la interacción nativa se cancela de inmediato.
   - Además, los bordes de redimensionamiento de 7px en Slint estaban dentro del viewport escalado, solapándose con los eventos de arrastre del fondo.

---

## 4. ¿Por qué en Discover hay que hacer doble clic en las películas?

### 4.1. Análisis del Código
Al inspeccionar [app/src/models/discover.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/discover.rs#L321-L363) frente a [board.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/board.rs#L41-L54) y [library.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/library.rs#L252-L265):

* **En Board y Library**:
  ```rust
  ui.on_board_item_selected({
      move |id, media_type, video_id| {
          open_details_route(&ui, &runtime, &navigation, &id); // <-- Entra a detalles con 1 solo clic
          load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
      }
  });
  ```
* **En Discover**:
  ```rust
  ui.on_discover_item_selected({
      move |id, media_type, video_id| {
          // <-- SOLO selecciona la película para mostrar el panel lateral de previsualización (preview panel)
          navigation.dispatch(NavigationIntent::SelectDiscoverPreview { media_id: id.clone() });
          load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
      }
  });

  ui.on_discover_item_activated({
      move |id, media_type, video_id| {
          // <-- Abre la pantalla completa de detalles SOLO en double_click / activate
          crate::models::details::open_details_route(&ui, &runtime, &navigation, &id);
      }
  });
  ```

### 4.2. Razón de Diseño vs Experiencia de Usuario
Discover fue programado originalmente clonando el comportamiento de Stremio Web oficial (versión web/desktop clásica), donde al hacer 1 clic se abre el panel lateral derecho con la sinopsis, año, director y botón "Play", y con doble clic se va a la ficha completa. Sin embargo, para un usuario de escritorio acostumbrado al flujo directo de Board o Library, tener que hacer doble clic se siente inconsistente y poco intuitivo.

---

## 5. Solución y Plan de Corrección Inmediato

1. **Eliminar cualquier regla KWin forzada (`kwinrulesrc`) que afecte a la ventana principal**:
   - Ya se limpió la sección `[stremio-pip]` de `~/.config/kwinrulesrc` y se ejecutó la reconfiguración de KWin.
   - En el código Rust ([player_features.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/player_features.rs)), no se debe inyectar `title=Stremio Oxide` ni forzar `decorationpolicy=none` de forma global persistente para la clase `stremio`. En su lugar, el cambio de decoración debe ser puramente dinámico a nivel de ventana Wayland (`window.set_decorations(!pip_active)`).
2. **Garantizar que la ventana normal conserve siempre su barra de título y no quede "Above"**:
   - `window.set_window_level(WindowLevel::Normal)` al salir de PiP.
   - `no-frame: false` en la ventana principal cuando `!player-pip-active`.
3. **Restaurar redimensionamiento y movimiento fluido en PiP**:
   - Asegurar que `window.drag_window()` y `window.drag_resize_window()` se invoquen de forma limpia sin bloqueos de KWin.
   - Si se cambia el título de la ventana dinámicamente cuando está en PiP a `"PiP"` o `"Stremio PiP"`, KWin puede aplicar reglas exclusivas a esa ventana sin tocar la ventana principal cuando no está en PiP.
4. **Discover con 1 solo clic**:
   - En `on_discover_item_selected`, abrir directamente la ficha de detalles (`open_details_route`) igual que en Board y Library, eliminando la fricción del doble clic.
