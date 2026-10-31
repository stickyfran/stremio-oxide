# Roadmap Blindado: UI/UX, Reproducción y Arquitectura - Stremio Oxide

Este documento consolida el diagnóstico de causas raíz, la auditoría arquitectónica inicial y los 4 ajustes finos de blindaje para resolver los 7 problemas críticos de experiencia de usuario y reproducción en **Stremio Oxide**. Ha sido diseñado como especificación técnica completa y auto-contenida para su implementación o auditoría por cualquier ingeniero o agente de IA.

---

## Índice de Contenidos

1. [Especificación Técnica de las 7 Secciones](#especificación-técnica-de-las-7-secciones)
   - [1. Fallas en Carga de Trailers y Playback Resiliente](#1-fallas-en-carga-de-trailers-y-playback-resiliente)
   - [2. Indicador Visual de Selección en Discover y Library](#2-indicador-visual-de-selección-en-discover-y-library)
   - [3. Animación Skeleton Shimmer y Corte de Repintado a 60 FPS](#3-animación-skeleton-shimmer-y-corte-de-repintado-a-60-fps)
   - [4 y 5. Estabilización de Streams y Eliminación de Flickering](#4-y-5-estabilización-de-streams-y-eliminación-de-flickering)
   - [6. Contenedor de Trailer en la Vista de Detalles (Screen Gigante)](#6-contenedor-de-trailer-en-la-vista-de-detalles-screen-gigante)
   - [7. Menú Contextual Reactivo en Tarjetas de Home](#7-menú-contextual-reactivo-en-tarjetas-de-home)
2. [Matriz de Riesgos y Puntos Ciegos Superados](#matriz-de-riesgos-y-puntos-ciegos-superados)
3. [Estrategia de Implementación por Fases](#estrategia-de-implementación-por-fases)

---

## Especificación Técnica de las 7 Secciones

### 1. Fallas en Carga de Trailers y Playback Resiliente

#### Causas Raíz Identificadas
1. **Destrucción por `rebuild()`**: Al hidratar el meta inicial se asocia el trailer, pero en cuanto los addons responden con los torrents, el event loop ejecuta `playback_selections.rebuild()`, el cual ejecuta `*current = next_entries`, purgando el trailer si `meta_items` no está en estado `Ready` en ese instante preciso.
2. **Desincronización de Meta ID**: Si se guarda un trailer genérico sin validar el ID, cambiar rápidamente de una Película A a una Película B puede causar que se reproduzca el trailer de la película anterior.
3. **Ausencia de `yt-dlp` o Bloqueo 403 de YouTube**: YouTube exige entornos de ejecución JS para descifrado de firmas y bloquea peticiones sin PO Tokens. Si `yt-dlp` falla o no está en el sistema, MPV aborta silenciosamente dejando la pantalla en negro o cargando indefinidamente.

#### Solución Arquitectónica Blindada
- **Registro persistente desacoplado (`pinned_trailer`)**:
  En `app/src/playback.rs`, crear un campo dedicado:
  ```rust
  pub struct PinnedTrailer {
      pub meta_id: String,
      pub selection: RegisteredSelection,
  }
  // En PlaybackSelections:
  pinned_trailer: RwLock<Option<PinnedTrailer>>,
  ```
  - Al hidratar el meta o solicitar el trailer, almacenar `PinnedTrailer` validando que `meta_id` coincida con la ruta actual.
  - Al cambiar de película/serie (`details_route_id`), limpiar atómicamente el `pinned_trailer` para evitar cross-playback de trailers antiguos.
  - Al invocar `playback_selections.resolve(id)`: si `id == "trailer"` o `id.starts_with("trailer:")`, resolver primero contra `pinned_trailer` antes de reportar expiración.
- **Doble Fallback de Reproducción**:
  - En `playback-mpv/src/actor.rs`: configurar `ytdl-raw-options=extractor-args="youtube:player_client=android,web"`.
  - En `app/src/mpv_integration.rs`: escuchar el evento `mpv_event::EndFile` cuando la reproducción termine por error. Si el stream corresponde a un video de YouTube (`StreamSource::YouTube { yt_id }`), cambiar el estado de la UI a un banner informativo y proveer fallback directo al navegador del sistema con `open::that(format!("https://www.youtube.com/watch?v={yt_id}"))`.

---

### 2. Indicador Visual de Selección en Discover y Library

#### Causas Raíz Identificadas
1. `MediaCard` posee la propiedad `in property <bool> selected: false;`, pero `MediaCardGridRow` (`media_carousel.slint`) no la propaga ni cuenta con `selected-id`.
2. `discover.slint` y `library.slint` no exponen `in property <string> selected-id: ""`.
3. `MediaCard` agrupaba la selección dentro de `highlighted` usando el mismo borde blanco/primario que el hover, imposibilitando distinguir entre la tarjeta seleccionada en el preview lateral y la tarjeta sobre la que reposa el cursor.

#### Solución Arquitectónica Blindada
1. **Diferenciación visual estricta**:
   En `app/ui/components/media_card.slint`:
   - Separar el contorno de selección del contorno de hover:
     ```slint
     // Anillo persistente de selección activa (Theme.accent violeta/acento)
     if (root.selected) : Rectangle {
         x: root.unit - 0.25 * root.unit;
         y: root.unit - 0.25 * root.unit;
         width: root.poster-width + 0.5 * root.unit;
         height: root.poster-height + 0.5 * root.unit;
         border-radius: Theme.border-radius + 0.25 * root.unit;
         border-width: 0.25 * root.unit;
         border-color: Theme.accent;
         background: transparent;
     }

     // Contorno translúcido de hover pasajero
     if (card-button.has-hover && !root.selected) : Rectangle {
         x: root.unit - 0.15 * root.unit;
         y: root.unit - 0.15 * root.unit;
         width: root.poster-width + 0.3 * root.unit;
         height: root.poster-height + 0.3 * root.unit;
         border-radius: Theme.border-radius + 0.15 * root.unit;
         border-width: 0.15 * root.unit;
         border-color: Theme.primary-foreground.with-alpha(0.6);
         background: transparent;
     }
     ```
2. **Propagación completa de propiedades**:
   - En `media_carousel.slint` (`MediaCardGridRow`):
     ```slint
     in property <string> selected-id: "";
     // En el for de MediaCard:
     selected: card-item.id != "" && card-item.id == root.selected-id;
     ```
   - En `discover.slint` y `library.slint`: exponer `in property <string> selected-id: "";` y pasarla a cada `MediaCardGridRow`.
   - En `app.slint`: enlazar con `discover-preview-id` y `library-preview-id`.

---

### 3. Animación Skeleton Shimmer y Corte de Repintado a 60 FPS

#### Causas Raíz Identificadas
- En Slint, un timer o animación con `iteration-count: -1` mantiene el bucle de renderizado activo a 60 fps forzando un "hot loop" de repintado que eleva el uso de CPU y desgasta la batería.
- Cuando `root.item.poster.width == 0`, `MediaCard` mostraba un fondo estático negro sin vida.

#### Solución Arquitectónica Blindada
- **Desmontaje real del árbol de dibujo**:
  En `app/ui/components/media_card.slint`:
  ```slint
  poster-container := Rectangle {
      x: root.unit;
      y: root.unit;
      width: root.poster-width;
      height: root.poster-height;
      background: Theme.overlay-color;
      border-radius: Theme.border-radius;
      clip: true;

      // El bloque condicional IF desmonta físicamente el componente y apaga la animación al cargar la imagen
      if (root.item.poster.width == 0 && (!root.enable-lazy-loading || root.is-visible)) : SkeletonBlock {
          width: parent.width;
          height: parent.height;
          radius: Theme.border-radius;
      }

      if (root.item.poster.width > 0) : Image {
          source: root.item.poster;
          width: parent.width;
          height: parent.height;
          image-fit: cover;
          animate opacity { duration: 200ms; easing: ease-out; }
      }
  }
  ```
  Al usar `if root.item.poster.width == 0`, Slint elimina de la memoria gráfica el nodo de animación continua en cuanto llega el fotograma del póster.

---

### 4 y 5. Estabilización de Streams y Eliminación de Flickering

#### Causas Raíz Identificadas
1. **Desalineación**: Los botones `download-button` y `play-button` se insertaban con un `if (root.selected || area.has-hover)` en el layout horizontal. Al posar el puntero, se restaban 6.1 rem a la columna central de texto (`wrap: word-wrap`), alterando los saltos de línea y desalineando las filas.
2. **Flickering y Competencia de Click**: `area := TouchArea` envolvía toda la fila para ejecutar `root.play()`. Al pasar el ratón a los botones hijos, `area` perdía el hover destruyendo el bloque `if`, provocando un parpadeo a 60 fps y disparando clicks accidentales en `play()` al intentar presionar `download()`.
3. **Clicks Fantasma por Opacidad**: En Slint, un componente con `opacity: 0.0` sigue recibiendo clicks si no está acompañado de `visible: false`.

#### Solución Arquitectónica Blindada
En `app/ui/components/media_rows.slint` (`StreamRow`):
1. **Estructura en dos zonas hermanas sin envoltura conflictiva**:
   ```slint
   property <bool> row-hovered: main-click-area.has-hover || download-btn.has-hover || play-btn.has-hover;

   HorizontalLayout {
       padding-left: root.unit;
       padding-right: root.unit;
       spacing: root.unit;
       alignment: center;

       // Zona 1: Área de información de reproducción (TouchArea independiente)
       info-and-text := Rectangle {
           horizontal-stretch: 1;
           height: parent.height;

           main-click-area := TouchArea {
               mouse-cursor: pointer;
               clicked => { root.play(); }
               pointer-event(event) => {
                   if (event.button == PointerEventButton.right && event.kind == PointerEventKind.down) {
                       root.context-requested(self.absolute-position.x + self.mouse-x, self.absolute-position.y + self.mouse-y);
                   }
               }
           }

           HorizontalLayout {
               stream-info := Rectangle { width: 7.5 * root.unit; ... }
               VerticalLayout {
                   horizontal-stretch: 1;
                   Text { text: root.description; wrap: word-wrap; overflow: elide; }
               }
           }
       }

       if (root.score-label != "") : Rectangle { ... }

       // Zona 2: Acciones fijas (ancho reservado permanente + revelación combinada)
       actions-container := HorizontalLayout {
           width: 6.2 * root.unit; // Ancho constante permanente
           spacing: 0.5 * root.unit;
           alignment: center;
           opacity: (root.selected || root.row-hovered) ? 1.0 : 0.0;
           visible: self.opacity > 0.01; // Impide clicks fantasma cuando está oculto
           animate opacity { duration: 150ms; easing: ease-out; }

           download-btn := ButtonSurface {
               width: 2.8 * root.unit;
               height: 2.8 * root.unit;
               border-radius: self.width / 2;
               clicked => { root.download(); }
               Image { source: @image-url("../assets/images/icons/download.svg"); ... }
           }

           play-btn := ButtonSurface {
               width: 2.8 * root.unit;
               height: 2.8 * root.unit;
               border-radius: self.width / 2;
               background: Theme.success;
               clicked => { root.play(); }
               Image { source: @image-url("../assets/images/icons/play.svg"); ... }
           }
       }
   }
   ```
   **Resultado**:
   - El ancho de la descripción nunca se altera (cero desalineación).
   - `row-hovered` se mantiene en `true` al pasar del texto al botón (cero parpadeo).
   - Los clicks en `download` o `play` nunca colisionan con el fondo.

---

### 6. Contenedor de Trailer en la Vista de Detalles (Screen Gigante)

#### Causas Raíz Identificadas
- En `details.slint`, el trailer solo existía como un botón menor dentro de la barra de acciones inferior de `MetaPreview`, ocultándose por completo si el metadato inicial no listaba `trailer_streams`.
- No existía un elemento visual destacado tipo tarjeta 16:9 que invite a ver el trailer oficial en la vista principal.

#### Solución Arquitectónica Blindada
- **Nuevo componente `TrailerCardPreview` dentro de `info-scroll`**:
  En `app/ui/components/meta_preview.slint`:
  - Ubicado dentro del `ScrollView` vertical (`info-scroll`), garantizando que en pantallas pequeñas o laptops de 1366x768 nunca rompa el layout responsive ni tape el resumen de enriquecimiento.
  - Estructurado como un rectángulo 16:9 con imagen backdrop atenuada, bordes redondeados, botón central flotante de YouTube/Play y etiqueta "Ver Trailer Oficial":
    ```slint
    if (root.show-trailer) : Rectangle {
        width: min(parent.width, 28 * root.unit);
        height: self.width * (9.0 / 16.0);
        border-radius: Theme.border-radius;
        background: Theme.overlay-color;
        clip: true;

        Image { source: root.background-image; width: 100%; height: 100%; image-fit: cover; opacity: 0.45; }
        
        // Botón central de reproducción con hover
        trailer-card-touch := TouchArea {
            mouse-cursor: pointer;
            clicked => { root.trailer-requested(); }
            // Icono play + texto
        }
    }
    ```

---

### 7. Menú Contextual Reactivo en Tarjetas de Home

#### Causas Raíz Identificadas
1. En `board.slint`, `library-context-menu` estaba configurado únicamente como `section.is-continue-watching`, deshabilitando el click derecho en todos los catálogos de recomendaciones/tendencias.
2. `catalog_media_card` en `app/src/models/mod.rs` no cruzaba los ítems con `model.ctx.library`, por lo que `in_library` y `watched` quedaban siempre en falso.
3. Al mutar `model.ctx.library` (por ejemplo con `LibraryAction::ToggleAddRemove`), la arquitectura de Stremio Core no siempre re-proyecta de inmediato las secciones del Board en la UI, pudiendo dejar la tarjeta desactualizada hasta un refresco manual.

#### Solución Arquitectónica Blindada
1. **Cruce de datos en Rust**:
   En `app/src/models/board.rs` y `mod.rs`:
   - Al construir `catalog_media_card`, recibir el conjunto de IDs de la biblioteca (`&HashSet<String>`) o consultar `model.ctx.library.items.contains_key(&item.id)`.
   - Poblar `in_library: bool` y `show_checkmark: bool` (visto).
2. **Re-proyección forzada en el event loop**:
   En `app/src/callbacks.rs`:
   - Al recibir `board-toggle-library(id)` o `board-toggle-watched(id, watched)`:
     1. Despachar la acción correspondiente al runtime de `stremio-core`.
     2. Disparar inmediatamente una actualización en la fila del Board en Slint (`set_row_data` o actualización de flag) para que el icono de favoritos o el visto cambie en 0 ms.
3. **Opciones completas en el menú contextual (`MediaCard`)**:
   - `▶ Reproducir / Continuar` (`Play`)
   - `ℹ Ver detalles` (`Details`)
   - `♥ Agregar a Favoritos / Quitar de Biblioteca` (`Toggle Library`)
   - `✓ Marcar como visto / no visto` (`Mark as watched / unwatched`)
   - `✕ Descartar` (exclusivo para Continue Watching)

---

## Matriz de Riesgos y Puntos Ciegos Superados

| Riesgo Técnico Original | Punto Ciego Detectado en Auditoría | Solución de Blindaje Implementada |
| :--- | :--- | :--- |
| **Pérdida de trailer por `rebuild()`** | Purgado por sobreescritura de mapa de streams. | Aislamiento en `PinnedTrailer` con validación de `meta_id`. |
| **Colisión visual de tarjetas** | Mismo borde para selección activa y hover. | Borde `Theme.accent` para seleccionado vs contorno tenue para hover. |
| **Sobrecarga de CPU por Shimmer** | Animaciones infinitas siguen corriendo en nodos invisibles. | Bloque condicional `if root.item.poster.width == 0` que desmonta el nodo en Slint. |
| **Flickering y Clicks Fantasma** | Pérdida de hover al separar áreas y captura de clicks con `opacity: 0`. | `row-hovered` combinado entre hermanos + `visible: self.opacity > 0.01`. |
| **Desfase de estado en Home** | Board no se actualiza automáticamente al cambiar Library. | Cruce $O(1)$ con `ctx.library` + re-proyección forzada instantánea en el callback. |

---

## Estrategia de Implementación por Fases

```mermaid
flowchart TD
    subgraph Fase 1: Quick Wins Visuales en Slint
        A1[media_rows.slint: Estabilizar streams y eliminar flickering con row-hovered]
        A2[media_card.slint: Desmontar SkeletonBlock con if poster.width == 0]
        A3[media_carousel.slint: Conectar selected-id en MediaCardGridRow]
        A4[discover.slint / library.slint: Exponer selected-id y diferenciar borde con Theme.accent]
    end

    subgraph Fase 2: Modelos, Estado y Menú Contextual en Home
        B1[board.rs / mod.rs: Cruzar catalog_media_card con ctx.library]
        B2[board.slint / media_card.slint: Habilitar menu contextual universal con Toggle Library y Watched]
        B3[callbacks.rs: Conectar board-toggle-library y board-toggle-watched con mutacion reactiva]
        B4[meta_preview.slint / details.slint: Componente TrailerCardPreview dentro de info-scroll]
    end

    subgraph Fase 3: Robustez de Playback y MPV
        C1[playback.rs: Implementar PinnedTrailer persistente e independiente de rebuild]
        C2[event_loop.rs / callbacks.rs: Limpieza y validacion atomica de meta_id de trailer]
        C3[mpv_integration.rs: Captura de EndFile con error y fallback a navegador externo con open::that]
    end

    Fase 1 --> Fase 2 --> Fase 3
```

- **Fase 1**: Quick Wins visuales directamente en Slint (cero cambios en lógica Rust pesada, elimina de inmediato las molestias visuales de flickering, desalineación y selección).
- **Fase 2**: Integración de estado de Biblioteca en el Board y previsualización de Trailer en Detalles.
- **Fase 3**: Blindaje de backend para resolución de trailers y resiliencia ante bloqueos de YouTube.
