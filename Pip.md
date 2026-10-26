# Auditoría Técnica y Especificación de la Lógica de Picture-in-Picture (PiP)

Este documento contiene el análisis arquitectónico, el diagnóstico de fallas de eventos en Wayland/X11 y la especificación técnica definitiva con correcciones para evitar problemas de reentrancia, desincronización de estado, destrucción de nodos y errores de compilación por alcance de identificadores en Slint.

---

## 1. Arquitectura y Componentes Clave

| Componente | Archivo | Responsabilidad |
| :--- | :--- | :--- |
| **Interfaz PiP (Slint)** | [`app/ui/pages/player.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/player.slint) | Touch areas de arrastre central, 8 manijas de redimensión perimetrales, superposiciones (top bar y bottom bar), controles de reproducción, scrubber de progreso y atajos de teclado/mouse. |
| **Controlador Nativo (Rust)** | [`app/src/player_features.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/player_features.rs) | `PipController`: snapshot y restauración de dimensiones de ventana, modo sin bordes (`set_decorations(false)`), `WindowLevel::AlwaysOnTop`, interacción con el compositor KWin en Linux vía D-Bus/kwinrulesrc. |
| **Puente UI ↔ Winit (Rust)** | [`app/src/mpv_integration.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/mpv_integration.rs) | Callbacks de Slint a Rust: `on_player_toggle_pip`, `on_player_start_pip_drag`, `on_player_start_pip_resize`, `on_player_scale_pip`, y gestión del temporizador de autohide del cursor/controles. |
| **Eventos de Ventana** | [`app/src/window_events.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/window_events.rs) y [`app/src/window_hooks.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/window_hooks.rs) | Registro y multiplexación del filtro `on_winit_window_event`. |

---

## 2. Diagnóstico de Fallas (Causas Raíz)

1. **Grab del Compositor en Wayland/KWin y ausencia de `ButtonRelease`:**
   Al llamar a `window.drag_window()` o `window.drag_resize_window(dir)`, Winit emite la petición `xdg_toplevel::move` o `xdg_toplevel::resize`. El compositor (KWin 6) toma el control interactivo exclusivo del puntero enviando un `wl_pointer::leave` a la ventana. Cuando el usuario suelta el mouse al terminar de mover o redimensionar, KWin termina el grab internamente pero **no envía el evento `wl_pointer::button(released)` a la superficie de Slint**. Slint se queda en estado `mouse_pressed = true` y con el item anterior retenido en `mouse_grab_item`. Esto bloquea por completo la detección de nuevos clicks, hovers y manijas de redimensión.

2. **`start-pip-drag()` ejecutado inmediatamente en `down`:**
   Al llamarse en `PointerEventKind.down` sin umbral, un click rápido destinado a pausar/reproducir o consultar la barra activa inmediatamente el grab del compositor. Esto secuestra el puntero antes de emitir `clicked`, rompe el doble click para maximizar y congela el árbol de eventos.

3. **Perforación de eventos en layouts vacíos de los Overlays:**
   Los overlays de PiP están en `z: 220`, y el `pip-drag-touch` está en `z: 80`. En Slint, `Rectangle` y `HorizontalLayout` son transparentes a los eventos de puntero salvo que contengan explícitamente un `TouchArea`. Cualquier click en espaciadores o texto intermedio perfora hacia `pip-drag-touch` (z: 80), disparando el grab del compositor mientras el usuario intentaba interactuar con la barra.

4. **Invalidación de `pip-hovered` por Z-Index:**
   El `TouchArea` de arrastre (`z: 80`) pierde `has-hover` en cuanto el cursor se sitúa sobre cualquier botón de los overlays (`z: 220`).

---

## 3. Trampas Técnicas y Desafíos de Compilación Resueltos

### Trampa A: Reentrancia, Timing y Coordenadas en la Liberación Forzada de Rust
- **Problema:** Un despacho sincrónico de `PointerReleased` dentro de `on_player_start_pip_drag` se produce con el borrow del árbol de items de Slint activo (conflicto de reentrancia) y antes de que el socket de Wayland tramite la petición de movimiento con su serial. Además, emitir en `(0.0, 0.0)` cae fuera del área activa del video debido al margen perimetral de 7 px reservado para las manijas de redimensión.
- **Solución:** Utilizar `slint::Timer::single_shot(16ms)` tanto en arrastre como en las 8 manijas de redimensión, emitiendo `PointerReleased` en coordenadas seguras `(15.0, 15.0)` seguido de `PointerExited`.

### Trampa B: Disparo Colateral de `toggle-pause()` Fantasma
- **Problema:** En Slint, al recibir el `PointerReleased` sintético de 16 ms en `(15.0, 15.0)`, el `TouchArea` dispara primero `pointer-event(PointerEventKind.up)` y a continuación evalúa `clicked`. Si `pip-is-dragging` se limpia en `up`, el callback `clicked => { if (!pip-is-dragging) ... }` evalúa a `true`, provocando que la ventana pause o reproduzca el video cada vez que se inicia un arrastre.
- **Solución:** No limpiar `pip-is-dragging` en el evento `up`. Mantenerlo activo durante el ciclo del click y limpiarlo únicamente al iniciar una nueva pulsación (`down`) o al término de la evaluación de `clicked`.

### Trampa C: Alcance de Identificadores (Scope) y Destrucción de Nodos en Slint
- **Problema:** En el compilador de Slint, cada bloque `if (cond) : Rectangle { ... }` genera un subcomponente aislado. Si una barra intenta referenciar directamente el ID de un `TouchArea` ubicado en otro bloque `if`, el compilador falla con error `Unknown element`. Asimismo, condicionar el montaje de las barras al hover destruye y recrea los nodos del layout continuamente.
- **Solución:** Declarar propiedades globales a nivel raíz (`pip-top-hovered` y `pip-bottom-hovered`). Los `TouchArea` escudo actualizan estas propiedades en su handler `changed has-hover`. Las barras se mantienen instanciadas con `if (pip-active)` y controlan su visibilidad evaluando `visible: root.pip-hovered || root.pip-top-hovered || root.pip-bottom-hovered || root.paused || root.controls-visible;`.

### Trampa D: Arrastre Natural desde la Barra Superior
- **Problema:** Si el `TouchArea` de la barra superior solo absorbe eventos, el usuario no puede mover la ventana agarrándola del título como en cualquier reproductor nativo.
- **Solución:** Dotar a `top-bar-touch` del mismo detector de umbral (> 4 px), permitiendo arrastrar desde la cabecera sin interferir con los botones de maximizar o cerrar. La barra inferior se mantiene como escudo puro sin arrastre para evitar desplazamientos accidentales al usar el scrubber.

---

## 4. Código Actual de Referencia

### 4.1 Slint UI ([`app/ui/pages/player.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/player.slint))

```slint
// Declaración de propiedades y callbacks
in-out property <bool> pip-active: false;
property <bool> pip-hovered: false;
property <bool> pip-drag-active: false;

callback toggle-pip();
callback start-pip-drag();
callback start-pip-resize(int);
callback scale-pip(float);

// Contenedor de Arrastre y Manijas de Redimensión
if (pip-active) : Rectangle {
    width: 100%;
    height: 100%;
    z: 80;

    pip-drag-touch := TouchArea {
        x: 7px;
        y: 7px;
        width: parent.width - 14px;
        height: parent.height - 14px;
        mouse-cursor: default;
        changed has-hover => {
            root.pip-hovered = self.has-hover;
        }
        pointer-event(event) => {
            if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
                controls-visible = true;
                root.activity();
                root.start-pip-drag();
            }
            if (event.kind == PointerEventKind.move) {
                controls-visible = true;
                root.activity();
            }
        }
        clicked => {
            root.activity();
            root.toggle-pause();
        }
        double-clicked => {
            root.toggle-pip();
        }
        scroll-event(event) => {
            if (event.delta-y > 0) {
                root.scale-pip(1.10);
            } else if (event.delta-y < 0) {
                root.scale-pip(0.90);
            }
            accept
        }
    }

    // 8 Manijas perimetrales de redimensión (ejemplo este)
    TouchArea {
        z: 120;
        x: parent.width - 7px;
        y: 14px;
        width: 7px;
        height: parent.height - 28px;
        mouse-cursor: ew-resize;
        pointer-event(event) => {
            if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
                root.start-pip-resize(0);
            }
        }
    }
}
```

### 4.2 Rust ([`app/src/mpv_integration.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/mpv_integration.rs))

```rust
ui.on_player_start_pip_drag({
    let weak = ui.as_weak();
    move || {
        let Some(ui) = weak.upgrade() else { return; };
        let _ = ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });
    }
});

ui.on_player_start_pip_resize({
    let weak = ui.as_weak();
    move |direction_idx| {
        let Some(ui) = weak.upgrade() else { return; };
        use slint::winit_030::winit::window::ResizeDirection;
        let dir = match direction_idx {
            0 => ResizeDirection::East,
            1 => ResizeDirection::West,
            2 => ResizeDirection::South,
            3 => ResizeDirection::North,
            4 => ResizeDirection::SouthEast,
            5 => ResizeDirection::SouthWest,
            6 => ResizeDirection::NorthWest,
            7 => ResizeDirection::NorthEast,
            _ => return,
        };
        let _ = ui.window().with_winit_window(|window| {
            let _ = window.drag_resize_window(dir);
        });
    }
});
```

---

## 5. Especificación Técnica Definitiva para la Implementación

### Paso 1: Declaración de Propiedades y Arrastre con Umbral en `player.slint`

En el componente raíz de `player.slint`, declarar las propiedades de tracking y hover:

```slint
// Propiedades de estado PiP en root:
property <bool> pip-top-hovered: false;
property <bool> pip-bottom-hovered: false;
property <length> pip-press-x: 0px;
property <length> pip-press-y: 0px;
property <bool> pip-is-pressed: false;
property <bool> pip-is-dragging: false;
```

Actualizar `pip-drag-touch` para implementar el umbral y prevenir el toggle-pause accidental:

```slint
pip-drag-touch := TouchArea {
    x: 7px;
    y: 7px;
    width: parent.width - 14px;
    height: parent.height - 14px;
    mouse-cursor: default;
    changed has-hover => {
        root.pip-hovered = self.has-hover;
    }
    pointer-event(event) => {
        if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
            pip-press-x = self.mouse-x;
            pip-press-y = self.mouse-y;
            pip-is-pressed = true;
            pip-is-dragging = false; // Se resetea al iniciar una nueva pulsación
            controls-visible = true;
            root.activity();
        }
        if (event.kind == PointerEventKind.move) {
            controls-visible = true;
            root.activity();
            if (pip-is-pressed && !pip-is-dragging) {
                let dx = self.mouse-x - pip-press-x;
                let dy = self.mouse-y - pip-press-y;
                if (abs(dx) > 6px || abs(dy) > 6px) {
                    pip-is-dragging = true;
                    pip-is-pressed = false; // Limpieza inmediata antes de invocar Wayland
                    root.start-pip-drag();
                }
            }
        }
        if (event.kind == PointerEventKind.up || event.kind == PointerEventKind.cancel) {
            pip-is-pressed = false;
            // IMPORTANTE: No limpiar pip-is-dragging acá. Slint invoca `clicked`
            // inmediatamente después del evento `up`. Al mantenerlo en true,
            // `clicked` sabrá que se trató de un arrastre y no pausará el video.
        }
    }
    clicked => {
        if (!pip-is-dragging) {
            root.activity();
            root.toggle-pause();
        }
        pip-is-dragging = false; // Se limpia al finalizar la evaluación del click
    }
    double-clicked => {
        if (!pip-is-dragging) {
            root.toggle-pip();
        }
        pip-is-dragging = false;
    }
    scroll-event(event) => {
        if (event.delta-y > 0) {
            root.scale-pip(1.10);
        } else if (event.delta-y < 0) {
            root.scale-pip(0.90);
        }
        accept
    }
}
```

---

### Paso 2: Liberación Diferida en Rust (`mpv_integration.rs`)

En `on_player_start_pip_drag` y `on_player_start_pip_resize`, despachar `PointerReleased` y `PointerExited` con un `Timer::single_shot` de 16 ms sobre una posición lógica interior (`15.0, 15.0`):

```rust
ui.on_player_start_pip_drag({
    let weak = ui.as_weak();
    move || {
        let Some(ui) = weak.upgrade() else { return; };
        let _ = ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });

        // Liberación diferida: evita reentrancia durante el pointer-event de Slint
        // y permite que Wayland registre el serial antes de limpiar el grab.
        let weak_release = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
            if let Some(ui) = weak_release.upgrade() {
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(15.0, 15.0),
                    button: slint::platform::PointerEventButton::Left,
                });
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerExited);
            }
        });
    }
});

ui.on_player_start_pip_resize({
    let weak = ui.as_weak();
    move |direction_idx| {
        let Some(ui) = weak.upgrade() else { return; };
        use slint::winit_030::winit::window::ResizeDirection;
        let dir = match direction_idx {
            0 => ResizeDirection::East,
            1 => ResizeDirection::West,
            2 => ResizeDirection::South,
            3 => ResizeDirection::North,
            4 => ResizeDirection::SouthEast,
            5 => ResizeDirection::SouthWest,
            6 => ResizeDirection::NorthWest,
            7 => ResizeDirection::NorthEast,
            _ => return,
        };
        let _ = ui.window().with_winit_window(|window| {
            let _ = window.drag_resize_window(dir);
        });

        // Liberación diferida de las manijas de redimensión
        let weak_release = weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
            if let Some(ui) = weak_release.upgrade() {
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(15.0, 15.0),
                    button: slint::platform::PointerEventButton::Left,
                });
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerExited);
            }
        });
    }
});
```

---

### Paso 3: Blindaje de Overlays, Scope Limpio y Arrastre desde la Cabecera

Instanciar las barras de forma persistente mientras dure el PiP (`if (pip-active)`), controlando su visibilidad mediante propiedades globales. Dotar a la cabecera de arrastre interactivo y a la barra inferior de escudo protector:

```slint
// Top Bar Overlay
if (pip-active) : Rectangle {
    x: 8px;
    y: 8px;
    width: parent.width - 16px;
    height: 32px;
    border-radius: 8px;
    background: Theme.scrim.with-alpha(0.85);
    border-width: 1px;
    border-color: Theme.divider;
    z: 220;
    visible: root.pip-hovered || root.pip-top-hovered || root.pip-bottom-hovered || root.paused || root.controls-visible;

    // 1. TouchArea escudo y arrastre de ventana desde la cabecera (primer hijo en z:220)
    top-bar-touch := TouchArea {
        width: 100%;
        height: 100%;
        changed has-hover => {
            root.pip-top-hovered = self.has-hover;
        }
        pointer-event(event) => {
            if (event.kind == PointerEventKind.down && event.button == PointerEventButton.left) {
                pip-press-x = self.mouse-x;
                pip-press-y = self.mouse-y;
                pip-is-pressed = true;
                pip-is-dragging = false;
                root.activity();
            }
            if (event.kind == PointerEventKind.move) {
                root.activity();
                if (pip-is-pressed && !pip-is-dragging) {
                    if (abs(self.mouse-x - pip-press-x) > 4px || abs(self.mouse-y - pip-press-y) > 4px) {
                        pip-is-dragging = true;
                        pip-is-pressed = false;
                        root.start-pip-drag();
                    }
                }
            }
            if (event.kind == PointerEventKind.up || event.kind == PointerEventKind.cancel) {
                pip-is-pressed = false;
            }
        }
        clicked => {
            pip-is-dragging = false;
        }
    }

    // 2. Layout hermano superior a top-bar-touch (recibe eventos prioritarios en botones)
    HorizontalLayout {
        padding-left: 10px;
        padding-right: 4px;
        spacing: 4px;
        alignment: center;

        Text {
            horizontal-stretch: 1;
            text: root.item-title != "" ? root.item-title : (root.series-name != "" ? root.series-name : @tr("Picture-in-picture"));
            color: Theme.text-primary;
            font-size: 11px;
            font-weight: 600;
            overflow: elide;
            vertical-alignment: center;
        }

        PipIconButton {
            icon-source: @image-url("../assets/images/icons/maximize.svg");
            tooltip: @tr("Restore window") + " (Escape)";
            clicked => {
                root.activity();
                root.toggle-pip();
            }
        }
        PipIconButton {
            icon-source: @image-url("../assets/images/icons/close.svg");
            tooltip: @tr("Close player");
            danger: true;
            clicked => {
                root.activity();
                root.close();
            }
        }
    }
}

// Bottom Bar Overlay
if (pip-active) : Rectangle {
    x: 8px;
    y: parent.height - self.height - 8px;
    width: parent.width - 16px;
    height: 58px;
    border-radius: 10px;
    background: Theme.scrim.with-alpha(0.88);
    border-width: 1px;
    border-color: Theme.divider;
    z: 220;
    visible: root.pip-hovered || root.pip-top-hovered || root.pip-bottom-hovered || root.paused || root.controls-visible;

    // 1. TouchArea escudo puro (sin arrastre, previene clicks perdidos hacia z:80)
    bottom-bar-touch := TouchArea {
        width: 100%;
        height: 100%;
        changed has-hover => {
            root.pip-bottom-hovered = self.has-hover;
        }
        pointer-event(event) => {
            if (event.kind == PointerEventKind.move || event.kind == PointerEventKind.down) {
                root.activity();
            }
        }
    }

    // 2. Layout interactivo del Scrubber y Controles
    VerticalLayout {
        padding-left: 8px;
        padding-right: 8px;
        padding-top: 4px;
        padding-bottom: 4px;
        spacing: 2px;

        // Scrubber
        HorizontalLayout {
            height: 16px;
            spacing: 6px;
            Text { text: root.elapsed-time-str; color: Theme.text-secondary; font-size: 10px; vertical-alignment: center; }
            Slider {
                horizontal-stretch: 1;
                min-width: 50px;
                height: 16px;
                value: root.progress;
                changed(val) => { root.activity(); root.seek(val); }
            }
            Text { text: root.total-time-str; color: Theme.text-muted; font-size: 10px; vertical-alignment: center; }
        }

        // Fila de botones
        HorizontalLayout {
            height: 28px;
            spacing: 4px;

            PipIconButton {
                icon-source: @image-url("../assets/images/icons/replay.svg");
                clicked => { root.activity(); root.seek-relative(-10); }
            }
            PipIconButton {
                icon-source: paused ? @image-url("../assets/images/icons/play.svg") : @image-url("../assets/images/icons/pause.svg");
                clicked => { root.activity(); root.toggle-pause(); }
            }
            PipIconButton {
                icon-source: @image-url("../assets/images/icons/skip-15-forward.svg");
                clicked => { root.activity(); root.seek-relative(10); }
            }
            if (is-series && has-next-episode) : PipIconButton {
                icon-source: @image-url("../assets/images/icons/next.svg");
                clicked => { root.activity(); root.play-next-episode(); }
            }

            Rectangle { horizontal-stretch: 1; }

            PipIconButton {
                icon-source: @image-url("../assets/images/icons/subtitles.svg");
                active: active-subtitle-idx >= 0;
                clicked => { root.activity(); root.toggle-subtitles(); }
            }
            PipIconButton {
                icon-source: @image-url("../assets/images/icons/audio.svg");
                clicked => { root.activity(); root.cycle-audio(); }
            }
            PipIconButton {
                icon-source: @image-url("../assets/images/icons/maximize.svg");
                clicked => { root.activity(); root.toggle-pip(); }
            }
        }
    }
}
```
