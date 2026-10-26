# Roadmap Técnico y Especificación de Navegación: UiNext.md

## 1. Resumen Ejecutivo y Objetivos

El presente documento define la reingeniería integral del sistema de navegación, previsualización de contenidos y enriquecimiento de metadatos en **Stremio Native**.

Esta versión corrige de raíz las contradicciones de Layout Shift (CLS), carreras asíncronas en red, colisiones entre instancias de MPV y omisiones funcionales del reproductor de trailers.

---

### 1.1. Problemas Detectados en la Implementación Actual

1. **Hover Preview Flotante Obsoleto (`TrailerHoverPopup`)**:
   - El popup se abre automáticamente tras 1200ms de posar el puntero del mouse sobre las tarjetas (`media_card.slint`).
   - Causa distracción visual, tapa tarjetas contiguas y pone a decodificar video de fondo en MPV sin una intención deliberada del usuario.
2. **Navegación Discrepante en Discover**:
   - En Stremio oficial (Web y Desktop v4/v5), hacer click en un título de Discover abre la barra lateral derecha manteniendo al usuario en el catálogo. En nuestra versión actual, el click salta bruscamente a la página completa de detalles (`DetailsPage`).
3. **Ausencia de Previsualización en Library**:
   - La biblioteca (`LibraryPage`) carece completamente de panel de previsualización lateral.
4. **Layout Shifts Severos (CLS) por Animación de Ancho en Grillas**:
   - Si se anima el `width` del panel lateral (ej. `animate width { duration: 200ms; }`), durante los 12 frames intermedios el layout de Slint recalcula continuamente el ancho de las tarjetas, reescala texturas y recalcula dinámicamente las columnas por fila, produciendo un tartamudeo visual (stutter) crítico.
5. **Saltos Verticales Internos por Datos Asíncronos**:
   - Cuando se selecciona una película, campos como `runtime`, `year` y las calificaciones de Rotten Tomatoes / Metacritic tardan entre 200ms y 600ms en resolverse. Si sus contenedores no tienen dimensiones rígidas pre-reservadas, empujan la sinopsis y los botones hacia abajo al llegar la respuesta.
6. **Colisión de Audio/GPU entre Reproductores**:
   - Si el usuario maximiza el trailer o hace click en "Show" para reproducir la película mientras el trailer del preview está activo, ambas instancias de MPV decodifican y reproducen audio simultáneamente si no se sincroniza la parada.

---

### 1.2. Objetivos y Especificaciones de la Solución

- **Eliminar el Hover Preview**: Suprimir los temporizadores de hover y el popup flotante en tarjetas.
- **Barra Lateral Derecha Unificada (MediaPreviewDrawer)**:
  - **En Discover**: El panel lateral derecho está **permanentemente abierto** (idéntico a Stremio oficial). Al cargar el catálogo, selecciona por defecto el primer ítem. De esta forma, el ancho de la grilla de Discover nunca oscila y el CLS es estrictamente **0.00**.
  - **En Library**: El panel cuenta con un toggle en la barra de herramientas para ocultarse si el usuario desea explorar la colección en pantalla completa. La conmutación de ancho es **instantánea** (1 solo frame de reflujo, sin animación de `width`) y la transición visual se realiza únicamente mediante `opacity` en 120ms.
- **Zero Layout Shifts Internos**:
  - Contenedor de video de relación fija (16:9 estricto, $29 \times \text{unit}$ de ancho $\times 16.31 \times \text{unit}$ de alto).
  - Contenedor de Título/Logo con altura máxima garantizada ($5 \times \text{unit}$).
  - Fila de Runtime/Año con reserva de altura obligatoria ($1.4 \times \text{unit}$).
  - Fila de Calificaciones fijada ($2.8 \times \text{unit}$) con **placeholders skeleton** visibles mientras `ratings-loading` es `true`.
- **Reproductor de Trailer Interactivo**:
  - **Auto-play desactivado por defecto**, con toggle/switch para activarlo a gusto del usuario y persistencia en configuración.
  - **Click sobre el video para alternar Play/Pause** (`toggle-trailer-playback`).
  - **Barra de progreso (Scrubber)** interactiva en el video con visualizador de tiempo transcurrido / total (`0:34 / 2:15`).
  - **Scroll Seek (+/- 5 segundos)** mediante rueda del mouse o trackpad, con **debounce/cooldown en Rust de 150ms** para evitar saltos desmedidos en Wayland/touchpads de alta resolución.
  - **Botón de Maximizar**: Detiene el worker de preview y transfiere el stream resuelto junto con su timestamp actual (`time-pos`) al reproductor principal o al modo PiP flotante.
- **Tríada de Calificaciones (IMDb, Rotten Tomatoes, Metacritic)**:
  - Badges con logotipos vectoriales (`imdb.svg`, `rotten-tomatoes.svg`, `metacritic.svg`).
  - Resolución asíncrona blindada contra condiciones de carrera mediante validación de `current_media_id` antes de aplicar resultados.

---

## 2. Diagnóstico del Código Actual

### 2.1. El Hover Preview actual (`MediaCard`, `app.slint`, `trailer_popup.slint`)
En `app/ui/components/media_card.slint`:
- Las tarjetas arman un temporizador de hover que emite `hover-preview-requested`:
```slint
// Líneas 117-145 en media_card.slint
hover-preview-timer := Timer {
    interval: root.hover-preview-delay; // 1200ms
    triggered => {
        root.preview-emitted = true;
        root.hover-preview-requested(root.item.id, root.item.media-type, ...);
    }
}
```
- En `app/ui/app.slint` (líneas 2212-2248): Se monta `TrailerHoverPopup`, superponiendo un card flotante en coordenadas absolutas de la ventana (`anchor-x`, `anchor-y`).
- **Decisión**: Eliminar el temporizador de hover en `media_card.slint` y remover `TrailerHoverPopup` de `app.slint`. El worker decodificador de MPV en segundo plano (`PreviewPlayer`) se redirige a alimentar la barra lateral fija.

### 2.2. Manejo de Selección en Discover (`discover.rs` y `discover.slint`)
En `app/src/models/discover.rs` (líneas 320-334):
```rust
ui.on_discover_item_selected({
    let runtime = runtime.clone();
    let ui_weak = ui_weak.clone();
    let navigation = navigation.clone();
    move |id, media_type, video_id| {
        // ERROR: Salta directamente a la ruta completa de detalles
        if let Some(ui) = ui_weak.upgrade() {
            crate::models::details::open_details_route(&ui, &runtime, &navigation, &id);
        }
        load_meta_details_for_video(&runtime, id, Some(media_type), video_id);
    }
});
```
- El click debe despachar `NavigationIntent::SelectDiscoverPreview { media_id }`, actualizando los metadatos de la barra lateral sin abandonar Discover.
- La navegación completa a `DetailsPage` se reserva para el botón **"Show"** del drawer o mediante doble click.

### 2.3. Estado de la Biblioteca (`library.slint` y `library.rs`)
En `app/ui/pages/library.slint`:
- La página consta únicamente de un filtro superior (`SelectField`, botones de orden) y la grilla `MediaCardGridRow`. Carece de barra lateral.
- En `app/src/models/library.rs` (línea 252), cualquier click dispara directamente `open_details_route(&ui, &runtime, &navigation, &id)`.

### 2.4. Scrapers de Calificaciones Disponibles (`ratings.rs` y `meta_preview.slint`)
En `app/src/ratings.rs`:
- Ya existe la estructura `MediaRatings` con clientes asíncronos para Rotten Tomatoes y Metacritic vía JSON-LD y caché `moka` (`RATINGS_CACHE`).
- En `app/ui/components/meta_preview.slint` (líneas 350-438) ya están definidos los estilos de UI con:
  - `imdb.svg` con `Theme.imdb` (amarillo dorado).
  - `rotten-tomatoes.svg` con porcentaje.
  - `metacritic.svg` con `Theme.metacritic` (verde/amarillo característico del Metascore "M").

---

## 3. Arquitectura y Solución de Diseño

```
+----------------------------------------------------------------------------------------------------+
| STREMIO NATIVE - NAVEGACIÓN Y PREVIEW (UiNext)                                                    |
+----------------------------------------------------------------------------------------------------+
| [Catálogo / Grilla de Películas o Biblioteca]                    | [Barra Lateral Derecha (Drawer)]|
|                                                                  | +-----------------------------+ |
| +-------+  +-------+  +-------+  +-------+  +-------+            | | REPRODUCTOR DE TRAILER      | |
| |       |  |       |  |       |  |       |  |       |            | | (Ratio 16:9 Fijo - Zero CLS)| |
| | Card  |  | Card  |  | Card  |  | Card  |  | Card  |            | | [Auto: OFF]     [Vol] [🗖]  | |
| |       |  | (Sel) |  |       |  |       |  |       |            | | [▶ / ⏸ Click: Play/Pause]  | |
| +-------+  +-------+  +-------+  +-------+  +-------+            | | [===⚪--------------------] | |
|                                                                  | | 0:34 / 2:15   (Scroll Seek) | |
| +-------+  +-------+  +-------+  +-------+  +-------+            | +-----------------------------+ |
| |       |  |       |  |       |  |       |  |       |            | | Logo o Título (H: 5.0*unit) | |
| | Card  |  | Card  |  | Card  |  | Card  |  | Card  |            | +-----------------------------+ |
| |       |  |       |  |       |  |       |  |       |            | | 1h 48m  ·  2024 (H: 1.4*u)  | |
| +-------+  +-------+  +-------+  +-------+  +-------+            | +-----------------------------+ |
|                                                                  | | (★ 8.2)  (🍅 88%)  (M 76)   | |
|                                                                  | | [Skeletons si cargando]     | |
|                                                                  | +-----------------------------+ |
| [Library Toolbar]: [ 👁 Toggle Panel Lateral ]                  | | [Scroll: Sinopsis & Cast]   | |
| (Conmutación instantánea: 1 frame, sin animate width)             | +-----------------------------+ |
|                                                                  | | [Ver / Show]  [+ Lib]  [👁]  | |
+------------------------------------------------------------------+---------------------------------+
```

---

### 3.1. Estrategia Anti-Layout Shift (Zero CLS Real)

#### 3.1.1. Eliminación del animate width que rompe la grilla
- **En Discover**:
  - El drawer está **siempre presente** (`width: root.drawer-width`).
  - Al cargar los catálogos por primera vez, se selecciona y proyecta automáticamente el primer elemento de la primera fila.
  - La grilla de Discover nace con su ancho final pre-calculado. **Nunca cambia de tamaño ni re-calcula columnas**.
- **En Library**:
  - Para alternar entre pantalla completa (solo biblioteca) y vista con panel lateral, la propiedad `width` del drawer conmuta de forma **inmediata**:
    ```slint
    width: root.is-open ? root.drawer-width : 0px;
    // SIN "animate width" en el contenedor padre.
    ```
  - Para lograr suavidad visual sin castigar la GPU ni el renderizado de la grilla, se anima exclusivamente la **opacidad interna** del contenido del drawer:
    ```slint
    drawer-content := VerticalLayout {
        opacity: root.is-open ? 1.0 : 0.0;
        animate opacity { duration: 120ms; easing: ease-out; }
    }
    ```
  - Resultado: El recálculo de columnas de la grilla ocurre **una sola vez en un único frame**, eliminando por completo los 12 saltos continuos producidos por transiciones de ancho.

#### 3.1.2. Blindaje de Dimensiones Internas (Cero Saltos Verticales)
1. **Contenedor de Video 16:9 Rígido**:
   - `video-height: root.drawer-width * 0.5625;` (exactamente $16.31 \times \text{unit}$). Si el trailer no existe o está detenido, la caja mantiene su altura reservada con el backdrop/póster.
2. **Contenedor de Título/Logo**:
   - `height: 5.0 * root.unit;`. Aloja el logotipo con `image-fit: contain` o el texto del título con elipsis.
3. **Fila de Metadatos Básicos (Runtime / Año)**:
   - `height: 1.4 * root.unit;` obligatorio. Aunque `runtime` tarde 300ms en llegar desde la API, el espacio vertical ya está reservado y nada se mueve.
4. **Fila de Calificaciones con Skeletons**:
   - `height: 2.8 * root.unit;`. Mientras `ratings-loading` es `true`, los badges muestran rectángulos tenues con animación de pulso o placeholders `"--"`, impidiendo que los puntajes empujen la sinopsis al cargarse.

---

### 3.2. Controles del Reproductor de Trailer y Sincronización de MPV

#### 3.2.1. Auto-Play con Configuración Persistente
- Campo en `Config`: `pub trailer_autoplay: bool` (por defecto `false`).
- Si `trailer_autoplay == false`: Al hacer click en una película, se muestra el póster en alta definición y un botón central de `Play`. El motor MPV no consume ciclos decodificando hasta que el usuario lo solicite.
- Si `trailer_autoplay == true`: El trailer inicia automáticamente al seleccionar la tarjeta.
- Toggle directo en la esquina superior del video (`Auto: ON / OFF`) para cambiar la preferencia al instante.

#### 3.2.2. Alternancia Play / Pause y Scrubber Interactivo
- **Click en el video**: Dispara `toggle-trailer-playback()`. Si está reproduciendo, pausa el video; si está pausado, reanuda.
- **Scrubber / Barra de Progreso**:
  - Línea de tiempo interactiva (`TouchArea` horizontal) que proyecta `video-progress` (0.0 a 1.0).
  - Al hacer click o arrastrar en la barra, invoca `seek-video-progress(ratio)` calculando la posición absoluta en segundos (`duration * ratio`).
  - Etiqueta de tiempo formateada (`video-time-label`, ej. `0:42 / 2:15`).

#### 3.2.3. Scroll Seek (+/- 5s) con Cooldown en Rust
- En el `TouchArea` del video:
  ```slint
  scroll-event(event) => {
      if (event.delta-y > 0px) {
          root.seek-video-relative(5.0);
      } else if (event.delta-y < 0px) {
          root.seek-video-relative(-5.0);
      }
      accept
  }
  ```
- **Protección contra saturación en Wayland / Touchpads**:
  En `PreviewPlayer::seek` (Rust), se implementa un filtro de tiempo (cooldown/debounce de 150ms) o acumulador de deltas:
  ```rust
  // En PreviewPlayer (Rust)
  pub fn seek(&self, offset_seconds: f64) {
      let mut last_seek = self.last_seek_instant.lock().unwrap();
      if last_seek.elapsed() < Duration::from_millis(150) {
          return; // Descarta micro-eventos acumulados por scroll suave
      }
      *last_seek = Instant::now();
      if let Some(controller) = self.controller() {
          log_worker_command(controller.seek(offset_seconds));
      }
  }
  ```

#### 3.2.4. Maximizar Trailer sin Fuga de Recursos y Parada al Navegar
- **Prevención de Doble Reproducción y Parada Global**:
  - Al pulsar el botón de maximizar (`maximize_trailer`), al navegar a detalles completos (`play_full_details` / `Show`), **o al cambiar de pestaña en la barra de navegación lateral izquierda** (Board, Discover, Library, Search, Addons, Settings), así como al tocar el botón `[X]` (`close-drawer`) en Library, se invoca **inmediatamente**:
    ```rust
    preview_player.dismiss(); // Detiene el worker de MPV del preview
    ```
    Esto garantiza que jamás queden decodificándose frames ni reproduciéndose audio del trailer en segundo plano al explorar otras secciones de la aplicación.
- **Transferencia de Estado y URL al Maximizar**:
  - Para evitar que el reproductor principal o la ventana PiP tengan que esperar 3 segundos volviendo a resolver la URL con `yt-dlp`, se extrae el URL del stream ya resuelto y el `time-pos` actual del preview, abriendo la reproducción exactamente en el segundo donde iba el usuario.

---

### 3.3. Prevención de Condiciones de Carrera en Calificaciones

En `app/src/ratings.rs`, para evitar que una petición lenta de la Película A sobreescriba las calificaciones de la Película B cuando el usuario hace clicks rápidos:

1. **Limpieza Sincrónica**: Apenas el usuario selecciona un nuevo ítem, en el hilo de UI se ejecuta de inmediato:
   ```rust
   ui.set_preview_drawer_rating_imdb(meta_item_imdb.into());
   ui.set_preview_drawer_rating_rt("".into());
   ui.set_preview_drawer_rating_metacritic("".into());
   ui.set_preview_drawer_ratings_loading(true);
   ```
2. **Validación de Identidad Asíncrona en el Hilo de UI**:
   El identificador actual de previsualización se almacena en un slot thread-safe (`CURRENT_PREVIEW_MEDIA_ID`). Para evitar cualquier ventana de carrera residual entre que la tarea de Tokio finaliza y el event loop de Slint procesa el mensaje, la verificación del identificador se realiza **dentro del closure en el hilo de UI**, justo en el instante de aplicar los datos:
   ```rust
   fn project_preview_if_current(ui_weak: &slint::Weak<MainWindow>, target_id: &str, ratings: &MediaRatings) {
       let target_id = target_id.to_owned();
       let rt = ratings.rotten_tomatoes.clone().unwrap_or_default();
       let mc = ratings.metacritic.clone().unwrap_or_default();
       let ui_weak = ui_weak.clone();

       let _ = slint::invoke_from_event_loop(move || {
           // Validar en el mismo hilo de la UI justo en el instante de escribir
           if let Ok(current) = CURRENT_PREVIEW_MEDIA_ID.lock() {
               if *current != target_id {
                   return; // Descartar: el usuario ya seleccionó otro título en el hilo de UI
               }
           }
           if let Some(ui) = ui_weak.upgrade() {
               ui.set_preview_drawer_rating_rt(rt.into());
               ui.set_preview_drawer_rating_metacritic(mc.into());
               ui.set_preview_drawer_ratings_loading(false);
           }
       });
   }
   ```

---

## 4. Código Concreto de `MediaPreviewDrawer.slint`

Este componente reemplaza el panel manual de `discover.slint` e implementa la barra de `library.slint`:

```slint
import { Theme } from "../theme.slint";
import { ButtonSurface, IconAction, PillButton } from "button.slint";
import { MetaStringLinks } from "meta_preview.slint";
import { ScrollView } from "std-widgets.slint";

export component MediaPreviewDrawer inherits Rectangle {
    in property <length> unit: Theme.root-font-size;
    in property <length> drawer-width: 29 * root.unit;
    in property <bool> is-open: true;

    // Metadatos
    in property <string> title: "";
    in property <image> logo;
    in property <image> poster;
    in property <string> year: "";
    in property <string> runtime: "";
    in property <string> description: "";
    in property <[string]> genres: [];
    in property <[string]> cast: [];
    in property <[string]> directors: [];

    // Calificaciones (The Big 3)
    in property <string> rating-imdb: "";
    in property <string> rating-rt: "";
    in property <string> rating-metacritic: "";
    in property <bool> ratings-loading: false;

    // Estado del trailer y video
    in property <image> video-frame;
    in property <bool> has-video-frame: false;
    in property <bool> video-loading: false;
    in property <bool> video-playing: false;
    in property <bool> video-muted: true;
    in property <bool> has-trailer: false;
    in property <bool> autoplay-enabled: false;
    in-out property <float> video-progress: 0.0; // 0.0 a 1.0
    in property <string> video-time-label: "";   // ej. "0:34 / 2:15"

    // Estado en Library
    in property <bool> in-library: false;
    in property <bool> is-watched: false;

    // Callbacks
    callback play-full-details();
    callback toggle-library();
    callback toggle-watched();
    callback close-drawer();
    callback toggle-trailer-playback();
    callback toggle-video-muted();
    callback toggle-autoplay();
    callback maximize-trailer();
    callback seek-video-relative(float);
    callback seek-video-progress(float);
    callback metadata-filter-selected(string);

    // Dimensiones fijas anti-layout shift (Zero CLS)
    property <length> video-height: root.drawer-width * 0.5625; // 16:9 exacto
    property <length> header-height: 5 * root.unit;
    property <length> meta-row-height: 1.4 * root.unit;
    property <length> ratings-height: 2.8 * root.unit;
    property <length> actions-height: 5.5 * root.unit;

    // Conmutación inmediata de width para no recalcular columnas 12 veces
    width: root.is-open ? root.drawer-width : 0px;
    visible: root.width > 0px;
    clip: true;
    background: Theme.modal-background;
    border-radius: Theme.border-radius;

    drawer-content := VerticalLayout {
        width: root.drawer-width;
        height: parent.height;
        spacing: 0px;
        opacity: root.is-open ? 1.0 : 0.0;
        animate opacity { duration: 120ms; easing: ease-out; }

        // =====================================================================
        // 1. ZONA SUPERIOR: REPRODUCTOR DE TRAILER (16:9 FIJO)
        // =====================================================================
        video-container := Rectangle {
            height: root.video-height;
            background: Theme.background-dark;
            clip: true;

            // Backdrop / Póster base
            if (root.poster.width > 0) : Image {
                width: parent.width;
                height: parent.height;
                source: root.poster;
                image-fit: cover;
                opacity: root.has-video-frame ? 0 : 0.45;
                animate opacity { duration: 150ms; }
            }

            // Frame decodificado por MPV
            if (root.has-video-frame) : Image {
                width: parent.width;
                height: parent.height;
                source: root.video-frame;
                image-fit: contain;
            }

            // Área interactiva de control: Click = Toggle Play/Pause, Scroll = Seek
            TouchArea {
                width: parent.width;
                height: parent.height;
                clicked => {
                    if (root.has-trailer) {
                        root.toggle-trailer-playback();
                    }
                }
                scroll-event(event) => {
                    if (event.delta-y > 0px) {
                        root.seek-video-relative(5.0);
                    } else if (event.delta-y < 0px) {
                        root.seek-video-relative(-5.0);
                    }
                    accept
                }
            }

            // Botón central de Play cuando está detenido o pausado
            if (!root.video-playing && root.has-trailer) : Rectangle {
                width: 4.5 * root.unit;
                height: 4.5 * root.unit;
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                border-radius: self.width / 2;
                background: Theme.background.with-alpha(0.7);
                border-width: 1px;
                border-color: Theme.accent;

                Image {
                    width: 2 * root.unit;
                    height: 2 * root.unit;
                    x: (parent.width - self.width) / 2 + 0.15 * root.unit;
                    y: (parent.height - self.height) / 2;
                    source: @image-url("../assets/images/icons/play.svg");
                    colorize: Theme.primary-foreground;
                }
            }

            // Indicador de carga / buffering
            if (root.video-loading && !root.has-video-frame) : Rectangle {
                width: parent.width;
                height: parent.height;
                background: Theme.background.with-alpha(0.5);

                Text {
                    text: @tr("Loading trailer...");
                    color: Theme.text-primary;
                    font-size: 0.9 * root.unit;
                    x: (parent.width - self.width) / 2;
                    y: (parent.height - self.height) / 2;
                }
            }

            // Barra superior de controles sobre el video
            HorizontalLayout {
                y: 0.75 * root.unit;
                x: 0.75 * root.unit;
                width: parent.width - 1.5 * root.unit;
                height: 2.5 * root.unit;
                alignment: space-between;

                // Toggle Autoplay
                ButtonSurface {
                    height: 2.2 * root.unit;
                    padding-left: 0.6 * root.unit;
                    padding-right: 0.6 * root.unit;
                    border-radius: 1.1 * root.unit;
                    background: root.autoplay-enabled ? Theme.accent : Theme.background.with-alpha(0.65);
                    clicked => { root.toggle-autoplay(); }

                    Text {
                        text: root.autoplay-enabled ? "Auto: ON" : "Auto: OFF";
                        color: Theme.primary-foreground;
                        font-size: 0.75 * root.unit;
                        font-weight: 600;
                        vertical-alignment: center;
                    }
                }

                HorizontalLayout {
                    spacing: 0.5 * root.unit;

                    // Mute / Unmute
                    if (root.video-playing) : IconAction {
                        width: 2.2 * root.unit;
                        height: 2.2 * root.unit;
                        unit: root.unit;
                        icon-size: 1.2 * root.unit;
                        surface-color: Theme.background.with-alpha(0.65);
                        icon-source: root.video-muted
                            ? @image-url("../assets/images/icons/volume-off.svg")
                            : @image-url("../assets/images/icons/volume-high.svg");
                        clicked => { root.toggle-video-muted(); }
                    }

                    // Maximizar a Reproductor Completo / PiP
                    if (root.has-trailer) : IconAction {
                        width: 2.2 * root.unit;
                        height: 2.2 * root.unit;
                        unit: root.unit;
                        icon-size: 1.2 * root.unit;
                        surface-color: Theme.background.with-alpha(0.65);
                        icon-source: @image-url("../assets/images/icons/maximize.svg");
                        clicked => { root.maximize-trailer(); }
                    }

                    // Cerrar Drawer
                    IconAction {
                        width: 2.2 * root.unit;
                        height: 2.2 * root.unit;
                        unit: root.unit;
                        icon-size: 1.2 * root.unit;
                        surface-color: Theme.background.with-alpha(0.65);
                        icon-source: @image-url("../assets/images/icons/close.svg");
                        clicked => { root.close-drawer(); }
                    }
                }
            }

            // Barra inferior con Scrubber interactivo y tiempo
            if (root.video-playing || root.video-progress > 0.0) : Rectangle {
                y: parent.height - 2.2 * root.unit;
                x: 0px;
                width: parent.width;
                height: 2.2 * root.unit;
                background: @linear-gradient(180deg, Theme.background.with-alpha(0) 0%, Theme.background.with-alpha(0.85) 100%);

                HorizontalLayout {
                    padding-left: 0.75 * root.unit;
                    padding-right: 0.75 * root.unit;
                    spacing: 0.75 * root.unit;
                    alignment: center;

                    // Scrubber Bar
                    scrubber-box := Rectangle {
                        horizontal-stretch: 1;
                        height: 0.4 * root.unit;
                        y: (parent.height - self.height) / 2; // Centrado vertical respecto al texto de tiempo
                        background: Theme.divider;
                        border-radius: self.height / 2;

                        Rectangle {
                            x: 0px;
                            y: 0px;
                            width: parent.width * Math.max(0.0, Math.min(1.0, root.video-progress));
                            height: parent.height;
                            background: Theme.accent;
                            border-radius: self.height / 2;
                        }

                        // Manija circular del scrubber
                        Rectangle {
                            x: parent.width * Math.max(0.0, Math.min(1.0, root.video-progress)) - self.width / 2;
                            y: (parent.height - self.height) / 2;
                            width: 0.9 * root.unit;
                            height: 0.9 * root.unit;
                            border-radius: self.width / 2;
                            background: Theme.primary-foreground;
                        }

                        TouchArea {
                            width: parent.width;
                            height: 1.5 * root.unit;
                            y: (parent.height - self.height) / 2;
                            clicked => {
                                if (scrubber-box.width > 0px) {
                                    root.seek-video-progress(Math.max(0.0, Math.min(1.0, self.mouse-x / scrubber-box.width)));
                                }
                            }
                        }
                    }

                    // Etiqueta de tiempo (ej. "0:34 / 2:15")
                    if (root.video-time-label != "") : Text {
                        text: root.video-time-label;
                        color: Theme.primary-foreground;
                        font-size: 0.75 * root.unit;
                        font-weight: 500;
                        vertical-alignment: center;
                    }
                }
            }
        }

        // =====================================================================
        // 2. CONTENIDO DESPLAZABLE INTERNO (Cero Saltos Estructurales)
        // =====================================================================
        ScrollView {
            vertical-stretch: 1;
            viewport-width: root.drawer-width;
            horizontal-scrollbar-policy: ScrollBarPolicy.always-off;

            VerticalLayout {
                padding-left: 2 * root.unit;
                padding-right: 2 * root.unit;
                padding-top: 1.5 * root.unit;
                padding-bottom: 1.5 * root.unit;
                spacing: 1.2 * root.unit;

                // Título o Logo (Altura fija para 0 CLS)
                Rectangle {
                    height: root.header-height;
                    background: transparent;

                    if (root.logo.width > 0) : Image {
                        width: parent.width;
                        height: parent.height;
                        source: root.logo;
                        image-fit: contain;
                        horizontal-alignment: left;
                    }

                    if (root.logo.width == 0) : Text {
                        width: parent.width;
                        height: parent.height;
                        text: root.title;
                        color: Theme.text-primary;
                        font-size: 1.6 * root.unit;
                        font-weight: 700;
                        wrap: word-wrap;
                        vertical-alignment: center;
                    }
                }

                // Fila de Metadatos Básicos (Runtime / Año) con altura fija obligatoria
                meta-row := Rectangle {
                    height: root.meta-row-height;
                    background: transparent;

                    HorizontalLayout {
                        spacing: 1.5 * root.unit;
                        alignment: start;
                        if (root.runtime != "") : Text {
                            text: root.runtime;
                            color: Theme.text-primary;
                            font-size: 0.95 * root.unit;
                            font-weight: 600;
                            opacity: 0.85;
                        }
                        if (root.year != "") : Text {
                            text: root.year;
                            color: Theme.text-secondary;
                            font-size: 0.95 * root.unit;
                        }
                    }
                }

                // =============================================================
                // 3. TRÍADA DE CALIFICACIONES (IMDb, Rotten Tomatoes, Metacritic)
                // =============================================================
                ratings-container := Rectangle {
                    height: root.ratings-height;
                    background: transparent;

                    HorizontalLayout {
                        spacing: 1.2 * root.unit;
                        alignment: start;

                        // IMDb
                        if (root.rating-imdb != "") : HorizontalLayout {
                            spacing: 0.4 * root.unit;
                            Image {
                                width: 2.2 * root.unit;
                                height: 1.2 * root.unit;
                                y: (parent.height - self.height) / 2;
                                source: @image-url("../assets/images/icons/imdb.svg");
                                colorize: Theme.imdb;
                                image-fit: contain;
                            }
                            Text {
                                text: root.rating-imdb;
                                color: Theme.text-primary;
                                font-size: 0.95 * root.unit;
                                font-weight: 600;
                                vertical-alignment: center;
                            }
                        }

                        // Rotten Tomatoes (con Skeleton mientras carga)
                        if (root.rating-rt != "") : HorizontalLayout {
                            spacing: 0.4 * root.unit;
                            Image {
                                width: 1.2 * root.unit;
                                height: 1.2 * root.unit;
                                y: (parent.height - self.height) / 2;
                                source: @image-url("../assets/images/icons/rotten-tomatoes.svg");
                                image-fit: contain;
                            }
                            Text {
                                text: root.rating-rt;
                                color: Theme.text-primary;
                                font-size: 0.95 * root.unit;
                                font-weight: 600;
                                vertical-alignment: center;
                            }
                        }
                        if (root.ratings-loading && root.rating-rt == "") : HorizontalLayout {
                            spacing: 0.4 * root.unit;
                            Rectangle {
                                width: 1.2 * root.unit;
                                height: 1.2 * root.unit;
                                y: (parent.height - self.height) / 2;
                                border-radius: 0.2 * root.unit;
                                background: Theme.card-border;
                            }
                            Rectangle {
                                width: 2.2 * root.unit;
                                height: 0.8 * root.unit;
                                y: (parent.height - self.height) / 2;
                                border-radius: 0.2 * root.unit;
                                background: Theme.card-border;
                            }
                        }

                        // Metacritic ("M") (con Skeleton mientras carga)
                        if (root.rating-metacritic != "") : HorizontalLayout {
                            spacing: 0.4 * root.unit;
                            Image {
                                width: 1.2 * root.unit;
                                height: 1.2 * root.unit;
                                y: (parent.height - self.height) / 2;
                                source: @image-url("../assets/images/icons/metacritic.svg");
                                colorize: Theme.metacritic;
                                image-fit: contain;
                            }
                            Text {
                                text: root.rating-metacritic;
                                color: Theme.text-primary;
                                font-size: 0.95 * root.unit;
                                font-weight: 600;
                                vertical-alignment: center;
                            }
                        }
                        if (root.ratings-loading && root.rating-metacritic == "") : HorizontalLayout {
                            spacing: 0.4 * root.unit;
                            Rectangle {
                                width: 1.2 * root.unit;
                                height: 1.2 * root.unit;
                                y: (parent.height - self.height) / 2;
                                border-radius: 0.2 * root.unit;
                                background: Theme.card-border;
                            }
                            Rectangle {
                                width: 1.8 * root.unit;
                                height: 0.8 * root.unit;
                                y: (parent.height - self.height) / 2;
                                border-radius: 0.2 * root.unit;
                                background: Theme.card-border;
                            }
                        }
                    }
                }

                // Sinopsis
                Text {
                    text: root.description;
                    color: Theme.text-muted;
                    font-size: 0.9 * root.unit;
                    wrap: word-wrap;
                }

                // Géneros
                if (root.genres.length > 0) : MetaStringLinks {
                    unit: root.unit;
                    label: @tr("LINKS_GENRES" => "Genres");
                    values: root.genres;
                    link-selected(value) => { root.metadata-filter-selected(value); }
                }

                // Cast y Directores
                if (root.cast.length > 0) : MetaStringLinks {
                    unit: root.unit;
                    label: @tr("CAST" => "Cast");
                    values: root.cast;
                    link-selected(value) => { root.metadata-filter-selected(value); }
                }
            }
        }

        // =====================================================================
        // 4. BARRA INFERIOR DE ACCIONES FIJAS
        // =====================================================================
        Rectangle {
            height: root.actions-height;
            background: Theme.modal-background;

            HorizontalLayout {
                padding: 1 * root.unit;
                spacing: 0.75 * root.unit;
                alignment: center;

                // Añadir / Quitar de Biblioteca
                IconAction {
                    width: 3.5 * root.unit;
                    height: 3.5 * root.unit;
                    unit: root.unit;
                    icon-size: 1.5 * root.unit;
                    surface-color: Theme.overlay;
                    active: root.in-library;
                    active-color: Theme.accent;
                    icon-source: root.in-library
                        ? @image-url("../assets/images/icons/checkmark.svg")
                        : @image-url("../assets/images/icons/add-to-library.svg");
                    clicked => { root.toggle-library(); }
                }

                // Marcar Visto / No Visto
                IconAction {
                    width: 3.5 * root.unit;
                    height: 3.5 * root.unit;
                    unit: root.unit;
                    icon-size: 1.5 * root.unit;
                    surface-color: Theme.overlay;
                    active: root.is-watched;
                    active-color: Theme.accent;
                    icon-source: root.is-watched
                        ? @image-url("../assets/images/icons/eye-off.svg")
                        : @image-url("../assets/images/icons/eye.svg");
                    clicked => { root.toggle-watched(); }
                }

                // Botón SHOW / Ver Completo (Navega a Details)
                PillButton {
                    horizontal-stretch: 1;
                    height: 3.5 * root.unit;
                    unit: root.unit;
                    label: @tr("SHOW" => "Show");
                    primary: true;
                    clicked => { root.play-full-details(); }
                }
            }
        }
    }
}
```

---

## 5. Plan de Implementación Paso a Paso

```
+-------------------------------------------------------------------------------------------------+
| ROADMAP DE IMPLEMENTACIÓN BLINDADO                                                             |
+-------------------------------------------------------------------------------------------------+
| FASE 1: Limpieza del Hover Popup en `media_card.slint`, `app.slint` y desacople de timers      |
| FASE 2: Creación de `MediaPreviewDrawer.slint` con métricas rígidas anti-CLS y Scrubber        |
| FASE 3: Enriquecimiento Asíncrono de Calificaciones en `ratings.rs` con validación de ID        |
| FASE 4: Cableado en `DiscoverPage` (Panel fijo permanente, click = preview, Show = details)    |
| FASE 5: Integración en `LibraryPage` (Toggle instantáneo en barra superior + persistencia)     |
| FASE 6: Manejo de Video en MPV (Play/Pause, Scroll seek con debounce de 150ms, Maximizar a PiP)|
| FASE 7: Verificación de compilación (`cargo check`), pruebas de memoria y testing visual        |
+-------------------------------------------------------------------------------------------------+
```

### Fase 1: Limpieza del Hover Preview
1. En `app/ui/components/media_card.slint`:
   - Eliminar `hover-preview-timer` y la propiedad `preview-armed`.
   - Suprimir la emisión de `hover-preview-requested` y `hover-preview-dismissed`.
2. En `app/ui/app.slint`:
   - Desmontar `TrailerHoverPopup` (líneas 2212-2248).
   - Eliminar propiedades y funciones del popup flotante (`hover-preview-anchor-x`, etc.).
3. En `app/src/event_loop.rs`:
   - Conservar `PreviewPlayer` para reutilizarlo como motor del nuevo drawer.

### Fase 2: Componente `MediaPreviewDrawer.slint`
1. Crear el archivo `app/ui/components/preview_drawer.slint` con el código Slint verificado de la Sección 4.
2. Confirmar que no incluya `animate width`, que posea reserva de altura en `meta-row` ($1.4 \times \text{unit}$), placeholders skeleton en calificaciones y el scrubber de video con `video-progress`.

### Fase 3: Integración de Calificaciones en `ratings.rs`
1. Incorporar el slot global seguro `CURRENT_PREVIEW_MEDIA_ID`.
2. En `fetch_and_project`, limpiar sincrónicamente la UI y encender `ratings_loading = true`:
   ```rust
   let _ = slint::invoke_from_event_loop(move || {
       if let Some(ui) = weak.upgrade() {
           ui.set_preview_drawer_rating_rt("".into());
           ui.set_preview_drawer_rating_metacritic("".into());
           ui.set_preview_drawer_ratings_loading(true);
       }
   });
   ```
3. Al recibir la respuesta asíncrona de Rotten Tomatoes y Metacritic, validar que `*CURRENT_PREVIEW_MEDIA_ID == target_id` antes de inyectar las cadenas en `ui`.

### Fase 4: Cableado en Discover (`discover.slint` y `discover.rs`)
1. Integrar `MediaPreviewDrawer` en `discover.slint`:
   - `is-open: true` siempre activo.
   - En el inicio del catálogo, seleccionar automáticamente el primer elemento si ninguno está activo.
2. En `app/src/models/discover.rs`:
   - Modificar `on_discover_item_selected` para que despache `NavigationIntent::SelectDiscoverPreview { media_id }` y sincronice metadatos, trailer y ratings.
   - Conectar el callback `play_full_details` al método existente `open_details_route(&ui, &runtime, &navigation, &id)`.

### Fase 5: Integración en Library y Toggle de Visibilidad
1. En `app/src/config.rs`:
   - Agregar campo de configuración `pub library_preview_enabled: bool` (por defecto `true`).
2. En `app/ui/pages/library.slint`:
   - Agregar propiedad `in-out property <bool> preview-enabled: true;`.
   - Incorporar un botón en la barra superior de Library para alternar `preview-enabled`.
   - Montar `MediaPreviewDrawer` con `is-open: root.preview-enabled` y sin `animate width`.
3. En `app/src/models/library.rs`:
   - Al hacer click en una tarjeta de la biblioteca:
     - Si `preview_enabled == true`: cargar metadatos en el drawer lateral.
     - Si `preview_enabled == false`: abrir directamente `open_details_route`.

### Fase 6: Controles de Video, Scroll Seek y Maximización a PiP
1. **Debounce en Scroll Seek**:
   - En `PreviewPlayer::seek`, filtrar micro-eventos sucesivos con un cooldown de 150ms para evitar saltos violentos en trackpads o ruedas de alta precisión.
2. **Alternancia Play / Pause**:
   - Conectar `toggle-trailer-playback` a `PreviewPlayer`: si el worker está reproduciendo, enviar comando de pausa de MPV; si está pausado, reanudar.
3. **Maximizar Trailer y Parada de Seguridad**:
   - Al hacer click en `maximize-trailer`:
     - Invocar `preview_player.dismiss()` de inmediato.
     - Lanzar el trailer en el reproductor principal o activar la ventana PiP flotante (`MainWindow.player-pip-active`) pasando la URL resuelta y el timestamp actual `time-pos`.
   - Al pulsar "Show" para ver la película completa, invocar igualmente `preview_player.dismiss()`.
   - **Parada Global al Cambiar de Pestaña o Cerrar Drawer**:
     - En el despachador central de rutas (`NavigationIntent` en `navigation.rs` o en `event_loop.rs`), cualquier cambio de pestaña en la barra de navegación lateral izquierda (Board, Discover, Library, Search, Addons, Settings) debe invocar automáticamente `preview_player.dismiss()`.
     - El callback `close-drawer` en Library también invoca `preview_player.dismiss()`. De este modo, nunca queda audio sonando de fondo ni video decodificando cuando el usuario cambia de sección o cierra el panel.

---

## 6. Matriz de Verificación y Criterios de Aceptación

| Requisito | Criterio de Verificación |
|---|---|
| **Eliminar Hover Preview** | Posar el puntero del mouse sobre cualquier póster no dispara popups ni decodifica video en segundo plano. |
| **Zero CLS en Discover** | El panel lateral de Discover está siempre montado con ancho fijo ($29 \times \text{unit}$). La grilla jamás re-calcula columnas al navegar entre títulos. |
| **Zero CLS en Library** | El toggle de Library conmuta el ancho en 1 frame exacto (sin `animate width`). Solo transiciona la opacidad (120ms). |
| **Cero Saltos Verticales** | La fila de Runtime/Año mide siempre $1.4 \times \text{unit}$. La fila de Calificaciones mide siempre $2.8 \times \text{unit}$ y muestra skeletons mientras `ratings-loading` es `true`. |
| **Play / Pause Interactivo** | Un click sobre el video en reproducción lo pausa. Otro click lo reanuda. |
| **Scrubber de Video** | Muestra el progreso actual (0.0 a 1.0) y la etiqueta de tiempo. Permite clickear para saltar a cualquier punto del trailer, perfectamente centrado verticalmente. |
| **Scroll Seek Controlado** | La rueda del mouse avanza (+5s) o retrocede (-5s). En trackpads Wayland no se disparan saltos desmedidos gracias al cooldown de 150ms en Rust. |
| **Prevención de Carreras en Ratings** | Hacer clicks sucesivos ultra-rápidos entre películas nunca muestra el puntaje de una película anterior en la película actual (validación en el hilo de UI dentro de `invoke_from_event_loop`). |
| **Sin Colisión de Reproductores** | Maximizar el trailer, hacer click en "Show", tocar `[X]` o cambiar de pestaña en la NavBar lateral izquierda detiene instantáneamente el worker del preview de MPV, sin solapamiento de audio ni fuga de GPU. |
| **Compilación y Tipos** | `cargo check -p stremio-native --bin stremio-native` finaliza con código 0 y sin errores. |
