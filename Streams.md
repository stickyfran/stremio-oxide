# Auditoría y Especificación Técnica: Lógica de Carga de Streams

Este documento contiene la arquitectura de extremo a extremo (End-to-End) de la carga de streams en Stremio Native, documenta el código fuente actual, diagnostica los fallos de parpadeo, condiciones de carrera y trampas de bindings en Slint, y establece la especificación técnica definitiva corregida (Opción 1: Progreso dinámico progresivo sin superposición) para su posterior revisión e implementación.

---

## 1. Arquitectura y Flujo de Datos de Streams

### 1.1 Diagrama del Ciclo de Vida

```
[Usuario selecciona Película o Episodio en UI]
                     │
                     ▼
          `models/details.rs`
 ├── Limpieza sincrónica de UI (`set_detail_stream_loading_count(1)`, `set_detail_streams([])`)
 └── Disparo de `ActionLoad::MetaDetails` (o `reload_stream_for_video`)
                     │
                     ▼
              `stremio-core`
 (Consulta HTTP en paralelo a todos los Addons con recurso "stream")
                     │
                     ▼
           `MetaDetails.streams`
 (Colección de recursos `Resource<Vec<Stream>>` con estado `Loadable`)
                     │
                     ▼
             `event_loop.rs`
 ├── `detail_stream_loading_count`: cuenta recursos en `Loadable::Loading`
 └── `playback.rs` (`rebuild`): transforma recursos listos (`Loadable::Ready`) en `StreamSelectionView`
                     │
                     ▼
             `app/ui/app.slint`
 (Propaga `detail-streams` y `detail-stream-loading-count` hacia `DetailsPage`)
                     │
                     ▼
         `app/ui/pages/details.slint`
   (Renderizado en 3 fases mutuamente excluyentes en la barra lateral)
```

---

## 2. Diagnóstico Técnico de Fallos y Condiciones de Carrera

### 2.1 Causa Raíz 1: Evaluación no excluyente de Estado Vacío y Carga
* **Ubicación:** [`app/ui/pages/details.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/details.slint#L515-L629).
* **Mecanismo:** El panel lateral evalúa `root.streams.length == 0` para mostrar el `EmptyState` ("No streams were found") y el botón "Install more addons". Al abrir una película o seleccionar un episodio, la lista arranca en 0 mientras los addons consultan la red. Al mismo tiempo, `root.stream-loading-count > 0` evalúa a `true`, dibujando la barra de carga debajo del cartel de vacío.
* **Efecto:** Ambos bloques se muestran apilados/superpuestos durante 1 a 3 segundos hasta que Torrentio o Real-Debrid devuelven datos, momento en el cual el cartel de vacío desaparece súbitamente.

### 2.2 Causa Raíz 2: Condición de Carrera en Transición de Episodios (1-Frame Flash)
* **Ubicación:** [`app/ui/pages/details.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/details.slint#L506-L510) y [`app/src/models/details.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/details.rs#L327-L373).
* **Mecanismo:** Cuando el usuario hace click en un episodio o en "Show streams" de una serie:
  1. En Slint, `root.in-stream-view = true;` se ejecuta sincrónicamente en el frame actual.
  2. `root.loading` es `false` porque los metadatos de la serie ya estaban en memoria.
  3. Si la actualización del contador en Rust es asíncrona o diferida a un hilo secundario de Tokio, Slint ve momentáneamente `in-stream-view == true`, `streams.length == 0` y `stream-loading-count == 0`.
* **Efecto:** Dispara un parpadeo visual de 1 a 2 frames del cartel "No streams found" o muestra momentáneamente los streams del episodio anterior.

### 2.3 Causa Raíz 3: Trampa de Mutación Interna y Ruptura de Bindings en Slint
* **Mecanismo:** En `DetailsPage` (`details.slint`), `streams` y `stream-loading-count` son propiedades declaradas como `in property`.
  * Asignarles valor dentro de `details.slint` (`root.streams = []` o `root.stream-loading-count = 1`) produce un error de compilación (`Cannot assign to input property`).
  * Si se cambian a `in-out`, pero en `app.slint` están conectadas mediante binding unidireccional `:`, una asignación interna en `details.slint` **destruye el binding reactivo**, provocando que `DetailsPage` deje de recibir futuras actualizaciones desde Rust.
* **Solución Arquitectónica:** No mutar `root.streams` ni `root.stream-loading-count` dentro de `details.slint`. La llamada al callback de Rust (`root.episode-changed` / `root.manual-episode-selected`) es 100% sincrónica y se ejecuta en el mismo hilo de la UI antes de que Slint dibuje el siguiente frame. Por lo tanto, asignar `ui.set_detail_stream_loading_count(1)` y `ui.set_detail_streams(slint::ModelRc::default())` directamente en el callback de Rust garantiza cero parpadeos sin comprometer los bindings de Slint.

### 2.4 Causa Raíz 4: Riesgo de Estado Colgado en Búsqueda Manual de Episodios
* **Mecanismo:** En `on_details_manual_episode_selected`, si se asignara `loading_count = 1` antes de resolver si el episodio existe, un episodio inválido (ej. temporada 99, episodio 99) provocaría que no se despache ninguna petición a `stremio-core`, dejando la UI congelada permanentemente en la Fase 1 ("Buscando streams...").
* **Solución:** Asignar `loading_count(1)` y vaciar streams solo cuando se confirme el `video_id` (o su fallback estándar `format!("{}:{season}:{episode}")`), y en caso de que sea nulo forzar explícitamente `ui.set_detail_stream_loading_count(0)` para que transicione de inmediato a la Fase 2 (`EmptyState`).

### 2.5 Causa Raíz 5: Comportamiento de `animate` vs `animation-tick()` en Slint
* **Mecanismo:** En Slint, la cláusula `animate <prop>` es un modificador de transición reactivo que únicamente actúa cuando el valor de la propiedad muta mediante un binding o asignación. Si se define un bloque con `animate opacity { duration: 800ms; ... }` pero la propiedad `opacity` permanece fija en 1.0, el compilador no genera un bucle de animación autónomo. El elemento queda congelado y estático.
* **Solución:** Las animaciones continuas puras (spinners, pulsos) en Slint se definen matemáticamente vinculando la propiedad a `animation-tick()`, por ejemplo: `opacity: 0.25 + 0.75 * abs(sin(animation-tick() / 800ms * 180deg));`.

---

## 3. Código Actual de Referencia

### 3.1 Slint UI Actual ([`app/ui/pages/details.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/details.slint#L515-L629))

```slint
// Estado vacío con superposición prematura
if ((!root.is-series || root.in-stream-view) && root.streams.length == 0) : VerticalLayout {
    vertical-stretch: 1;
    padding-top: root.unit;
    padding-bottom: root.unit;
    spacing: 1.25 * root.unit;
    alignment: center;

    if (root.is-series) : HorizontalLayout {
        height: 3.5 * root.unit;
        spacing: 0.75 * root.unit;
        alignment: center;

        NumberInput { width: 12.5 * root.unit; unit: root.unit; label: @tr("SEASON" => "Season"); value <=> root.manual-season; }
        NumberInput { width: 12.5 * root.unit; unit: root.unit; label: @tr("EPISODE" => "Episode"); value <=> root.manual-episode; }
    }

    if (root.is-series) : PillButton {
        width: 15 * root.unit;
        unit: root.unit;
        label: @tr("SIDEBAR_SHOW_STREAMS" => "Show streams");
        clicked => {
            root.manual-episode-selected(root.manual-season, root.manual-episode);
            root.in-stream-view = true;
        }
    }

    EmptyState {
        height: 16 * root.unit;
        unit: root.unit;
        icon-source: @image-url("../assets/images/icons/addons-outline.svg");
        title: @tr("NO_STREAM" => "No streams were found");
        message: @tr("INSTALL_MORE_ADDONS" => "For more streams, please install more addons");
    }

    PillButton {
        width: 16 * root.unit;
        unit: root.unit;
        label: @tr("ADDON_CATALOGUE_MORE" => "Install more addons");
        outlined: true;
        clicked => { root.open-addons(); }
    }
}

// Lista de streams
if ((!root.is-series || root.in-stream-view) && root.streams.length > 0) : ListView {
    vertical-stretch: 1;
    for stream in root.streams : stream-delegate := Rectangle {
        // StreamRow...
    }
}

// Indicador inferior que se apila debajo del estado vacío
if ((!root.is-series || root.in-stream-view) && root.stream-loading-count > 0) : VerticalLayout {
    height: 4 * root.unit;
    spacing: 0.75 * root.unit;
    Text {
        text: @tr("STREMIO_TV_STREAMS_STILL_LOADING" => "{} addon is still loading...", root.stream-loading-count);
        color: Theme.text-primary;
        font-size: root.unit;
        horizontal-alignment: center;
    }
    Rectangle {
        height: 0.3 * root.unit;
        border-radius: self.height / 2;
        background: Theme.accent;
    }
}
```

### 3.2 Manejo de Episodios en Rust ([`app/src/models/details.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/details.rs#L327-L373))

```rust
ui.on_details_episode_changed({
    let runtime = runtime.clone();
    move |episode_idx, video_id| {
        if let Ok(mut ep) = get_active_episode_idx().lock() {
            *ep = episode_idx as usize;
        }

        let rt = runtime.clone();
        let video_id = video_id.to_string();
        tokio::spawn(async move {
            reload_stream_for_video(&rt, video_id);
        });
    }
});

ui.on_details_manual_episode_selected({
    let runtime = runtime.clone();
    move |season, episode| {
        if episode < 1 || season < 0 {
            return;
        }
        let video_id = runtime.model().ok().and_then(|model| {
            let selected = model.meta_details.selected.as_ref()?;
            let ready_meta = model.meta_details.meta_items.iter().find_map(|resource| {
                match &resource.content {
                    Some(Loadable::Ready(meta)) if meta.preview.id == selected.meta_path.id => {
                        Some(meta)
                    }
                    _ => None,
                }
            });
            ready_meta
                .and_then(|meta| {
                    meta.videos.iter().find_map(|video| {
                        video.series_info.as_ref().and_then(|info| {
                            (info.season as i32 == season && info.episode as i32 == episode)
                                .then(|| video.id.clone())
                        })
                    })
                })
                .or_else(|| Some(format!("{}:{season}:{episode}", selected.meta_path.id)))
        });
        if let Some(video_id) = video_id {
            reload_stream_for_video(&runtime, video_id);
        }
    }
});
```

---

## 4. Especificación Técnica Definitiva (Opción 1)

### Paso 1: Blindaje Sincrónico en Rust sin Riesgo de Estado Colgado ([`app/src/models/details.rs`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/details.rs))

Capturar `weak_ui` y ejecutar la limpieza sincrónica en el mismo hilo antes del despacho a Tokio. En la selección manual, manejar la rama `else` para garantizar que si un video es nulo caiga en `loading_count(0)` y muestre el `EmptyState`:

```rust
// Episode changed callback
ui.on_details_episode_changed({
    let runtime = runtime.clone();
    let weak_ui = ui.as_weak();
    move |episode_idx, video_id| {
        if let Ok(mut ep) = get_active_episode_idx().lock() {
            *ep = episode_idx as usize;
        }

        // Blindaje sincrónico en el hilo de la UI: entra a Fase 1 antes del siguiente frame
        if let Some(ui) = weak_ui.upgrade() {
            ui.set_detail_stream_loading_count(1);
            ui.set_detail_streams(slint::ModelRc::default());
        }

        let rt = runtime.clone();
        let video_id = video_id.to_string();
        tokio::spawn(async move {
            reload_stream_for_video(&rt, video_id);
        });
    }
});

// Manual episode selected callback
ui.on_details_manual_episode_selected({
    let runtime = runtime.clone();
    let weak_ui = ui.as_weak();
    move |season, episode| {
        if episode < 1 || season < 0 {
            return;
        }

        let video_id = runtime.model().ok().and_then(|model| {
            let selected = model.meta_details.selected.as_ref()?;
            let ready_meta = model.meta_details.meta_items.iter().find_map(|resource| {
                match &resource.content {
                    Some(Loadable::Ready(meta)) if meta.preview.id == selected.meta_path.id => {
                        Some(meta)
                    }
                    _ => None,
                }
            });
            ready_meta
                .and_then(|meta| {
                    meta.videos.iter().find_map(|video| {
                        video.series_info.as_ref().and_then(|info| {
                            (info.season as i32 == season && info.episode as i32 == episode)
                                .then(|| video.id.clone())
                        })
                    })
                })
                .or_else(|| Some(format!("{}:{season}:{episode}", selected.meta_path.id)))
        });

        if let Some(video_id) = video_id {
            // Episodio válido: inicializar carga sincrónica y recargar
            if let Some(ui) = weak_ui.upgrade() {
                ui.set_detail_stream_loading_count(1);
                ui.set_detail_streams(slint::ModelRc::default());
            }
            reload_stream_for_video(&runtime, video_id);
        } else if let Some(ui) = weak_ui.upgrade() {
            // Episodio no encontrado: forzar 0 para mostrar inmediatamente la Fase 2 (EmptyState)
            ui.set_detail_stream_loading_count(0);
            ui.set_detail_streams(slint::ModelRc::default());
        }
    }
});
```

---

### Paso 2: Renderizado en 3 Fases Mutuamente Excluyentes en `details.slint`

En `details.slint`, las propiedades `streams` y `stream-loading-count` se mantienen intactas como `in property`. Los callbacks se invocan sin mutar propiedades locales para preservar íntegros los bindings con `app.slint`:

```slint
            // =========================================================================
            // FASE 1: CARGA INICIAL (0 streams recibidos y búsqueda en progreso)
            // =========================================================================
            if ((!root.is-series || root.in-stream-view)
                && root.streams.length == 0
                && (root.stream-loading-count > 0 || root.loading)) : VerticalLayout {
                vertical-stretch: 1;
                alignment: center;
                spacing: 1.5 * root.unit;
                padding: 2 * root.unit;

                // Animación de pulso continuo autónoma basada en animation-tick()
                HorizontalLayout {
                    alignment: center;
                    Rectangle {
                        width: 4 * root.unit;
                        height: 4 * root.unit;
                        border-radius: 2 * root.unit;
                        background: Theme.accent.with-alpha(0.15);

                        Rectangle {
                            x: (parent.width - self.width) / 2;
                            y: (parent.height - self.height) / 2;
                            width: 2.2 * root.unit;
                            height: 2.2 * root.unit;
                            border-radius: 1.1 * root.unit;
                            background: Theme.accent;
                            // Pulso continuo suave entre 0.25 y 1.0 sin timers externos
                            opacity: 0.25 + 0.75 * abs(sin(animation-tick() / 800ms * 180deg));
                        }
                    }
                }

                Text {
                    text: root.stream-loading-count > 0
                        ? @tr("Buscando streams en addons ({} pendientes)...", root.stream-loading-count)
                        : @tr("Cargando streams...");
                    color: Theme.text-primary;
                    font-size: 1.1 * root.unit;
                    font-weight: 500;
                    horizontal-alignment: center;
                }

                Text {
                    text: @tr("Consultando proveedores configurados...");
                    color: Theme.text-muted;
                    font-size: 0.9 * root.unit;
                    horizontal-alignment: center;
                }
            }

            // =========================================================================
            // FASE 2: ESTADO VACÍO GENUINO (Todos los addons terminaron y hay 0 streams)
            // =========================================================================
            if ((!root.is-series || root.in-stream-view)
                && root.streams.length == 0
                && root.stream-loading-count == 0
                && !root.loading) : VerticalLayout {
                vertical-stretch: 1;
                padding-top: root.unit;
                padding-bottom: root.unit;
                spacing: 1.25 * root.unit;
                alignment: center;

                if (root.is-series) : HorizontalLayout {
                    height: 3.5 * root.unit;
                    spacing: 0.75 * root.unit;
                    alignment: center;

                    NumberInput {
                        width: 12.5 * root.unit;
                        unit: root.unit;
                        label: @tr("SEASON" => "Season");
                        value <=> root.manual-season;
                        show-buttons: true;
                        has-explicit-minimum: true;
                        minimum: 0;
                    }
                    NumberInput {
                        width: 12.5 * root.unit;
                        unit: root.unit;
                        label: @tr("EPISODE" => "Episode");
                        value <=> root.manual-episode;
                        show-buttons: true;
                        has-explicit-minimum: true;
                        minimum: 1;
                    }
                }

                if (root.is-series) : PillButton {
                    width: 15 * root.unit;
                    unit: root.unit;
                    label: @tr("SIDEBAR_SHOW_STREAMS" => "Show streams");
                    clicked => {
                        root.manual-episode-selected(root.manual-season, root.manual-episode);
                        root.in-stream-view = true;
                    }
                }

                EmptyState {
                    height: 16 * root.unit;
                    unit: root.unit;
                    icon-source: @image-url("../assets/images/icons/addons-outline.svg");
                    title: @tr("NO_STREAM" => "No streams were found");
                    message: @tr("INSTALL_MORE_ADDONS" => "For more streams, please install more addons");
                }

                PillButton {
                    width: 16 * root.unit;
                    unit: root.unit;
                    label: @tr("ADDON_CATALOGUE_MORE" => "Install more addons");
                    outlined: true;
                    clicked => { root.open-addons(); }
                }
            }

            // =========================================================================
            // FASE 3: LISTA DE STREAMS DISPONIBLES (Progresiva)
            // =========================================================================
            if ((!root.is-series || root.in-stream-view) && root.streams.length > 0) : ListView {
                vertical-stretch: 1;
                horizontal-scrollbar-policy: ScrollBarPolicy.always-off;

                for stream in root.streams : stream-delegate := Rectangle {
                    private property <bool> matches-provider:
                        root.active-stream-provider == "All"
                            || stream.provider == root.active-stream-provider;
                    width: parent.width;
                    height: self.matches-provider ? 5.8 * root.unit : 0px;
                    clip: true;
                    background: transparent;

                    StreamRow {
                        visible: stream-delegate.matches-provider;
                        x: 0px;
                        y: 0px;
                        width: parent.width;
                        height: 5.6 * root.unit;
                        unit: root.unit;
                        name: stream.name;
                        description: stream.description;
                        score-label: root.ranking-mode == "Original" ? "" : (stream.score >= 0 ? "+" + stream.score : "" + stream.score);
                        score-details: stream.score-summary;
                        filtered: stream.filtered;
                        thumbnail: stream.thumbnail;
                        progress: stream.progress;
                        play => { root.play-stream(stream.id); }
                        context-requested(x, y) => {
                            root.stream-context-x = x;
                            root.stream-context-y = y;
                            root.stream-context-id = stream.id;
                            root.stream-context-open = true;
                        }
                    }
                }
            }

            // =========================================================================
            // BADGE DE CARGA EN SEGUNDO PLANO (Solo si ya hay streams pero faltan addons)
            // =========================================================================
            if ((!root.is-series || root.in-stream-view)
                && root.streams.length > 0
                && root.stream-loading-count > 0) : Rectangle {
                height: 2.8 * root.unit;
                border-radius: 6px;
                background: Theme.surface-elevated;
                border-width: 1px;
                border-color: Theme.divider;

                HorizontalLayout {
                    padding-left: root.unit;
                    padding-right: root.unit;
                    spacing: 0.75 * root.unit;
                    alignment: center;

                    // Indicador circular con centrado vertical explícito y pulso dinámico
                    Rectangle {
                        y: (parent.height - self.height) / 2;
                        width: 8px;
                        height: 8px;
                        border-radius: 4px;
                        background: Theme.accent;
                        opacity: 0.35 + 0.65 * abs(sin(animation-tick() / 700ms * 180deg));
                    }

                    Text {
                        text: @tr("Buscando más streams en {} addons...", root.stream-loading-count);
                        color: Theme.text-secondary;
                        font-size: 0.85 * root.unit;
                        vertical-alignment: center;
                    }
                }
            }
```

---

## 5. Checklist de Auditoría y Verificación

1. **Integridad de Bindings en Slint:** No se deben mutar `root.streams` ni `root.stream-loading-count` dentro de `details.slint`; se mantienen como `in property` con sus bindings unidireccionales desde `app.slint` intactos.
2. **Prevención de Estado Colgado:** En la selección manual de episodios, si el episodio ingresado no existe en los metadatos y no se genera una petición válida, el callback debe forzar `loading_count(0)` para permitir que la Fase 2 muestre el `EmptyState` correspondiente.
3. **Cero Parpadeos en Transición:** Al seleccionar un episodio de una serie, el callback sincrónico en Rust debe vaciar los streams y fijar `loading_count(1)` antes del despacho asíncrono, asegurando que la UI pase a la Fase 1 en el mismo frame sin mostrar el cartel "No streams found".
4. **Animación Fluida:** El pulso central de la Fase 1 y el indicador del badge de fondo deben animarse fluidamente a través de `animation-tick()`.
5. **Alineación Vertical del Badge:** El punto de 8 px debe quedar centrado verticalmente respecto a su texto dentro de su `HorizontalLayout`.
