# Roadmap Definitivo y Auditado: Carga de Catálogos al Arrancar desde `v0.99.7`

**Base:** tag `v0.99.7` (`3afcfde`) en rama `work/catalog-startup-v0.99.7`  
**Estado:** 2da auditoría profunda completada contra `v0.99.7` y `stremio-core`

---

## 1. Segunda Auditoría al Roadmap: 4 Trampas Nuevas Detectadas

Al volver a revisar paso a paso el roadmap anterior contra la implementación interna de `stremio-core` y `core-env` en `v0.99.7`, encontré **4 detalles críticos** que en la primera pasada no estaban bien cerrados:

### Trampa 1: En `v0.99.7` nunca se sincronizan los addons de la cuenta con la API al arrancar (`PullAddonsFromAPI` / `PullUserFromAPI`)
- **Hecho verificado en `v0.99.7`:**
  - En `app/src/main.rs` (`finish_startup`, líneas 751–1085) y en `app/src/callbacks.rs` (`trigger_initial_load`, líneas 682–728), **en ningún momento del arranque** se despacha `ActionCtx::PullAddonsFromAPI` ni `ActionCtx::PullUserFromAPI`.
  - Esas acciones solo existen en `app/src/window_events.rs:56` (`dispatch_focus_sync`), y ese hook tiene `last_focus_sync = Instant::now()` con un debounce de `2 segundos` (`FOCUS_SYNC_DEBOUNCE`), por lo que el evento `Focused(true)` inicial de la ventana **es ignorado**.
  - **Consecuencia real:** Si el usuario instaló/actualizó addons en otro dispositivo, o si en su perfil local quedó una lista vieja/incompleta, **al abrir la app nunca se actualiza la lista de addons desde la API de Stremio** salvo que el usuario desenfoque la ventana y la vuelva a enfocar después de 2 segundos.
- **Corrección en el Roadmap:**
  - En `trigger_initial_load` (`app/src/callbacks.rs`), además de cargar los catálogos con los addons que ya están en disco (para que la UI muestre contenido en 0ms), despachar también `Action::Ctx(ActionCtx::PullAddonsFromAPI)`, `Action::Ctx(ActionCtx::PullUserFromAPI { token: None })` y `Action::Ctx(ActionCtx::SyncLibraryWithAPI)`.
  - Cuando `PullAddonsFromAPI` responde (unos ~300ms después), si los addons cambiaron, `stremio-core` emite `Internal::ProfileChanged` → reconstruye `model.board.catalogs` y nuestro detector en `event_loop.rs` dispara automáticamente `LoadRange(0..6)` para los catálogos nuevos.

### Trampa 2: Qué pasa en `Discover` cuando `PullAddonsFromAPI` trae addons nuevos (`CatalogWithFilters` no recarga solo)
- **Hecho verificado en `stremio-core` (`catalog_with_filters.rs:230`):**
  - Ante `Internal::ProfileChanged`, `Board` (`CatalogsWithExtra`) reconstruye `self.catalogs`.
  - Pero `Discover` (`CatalogWithFilters`) ante `Internal::ProfileChanged` **solo** llama a `selectable_update(...)`: si `self.selected` era `None` (porque al arrancar el perfil aún no tenía addons de ese tipo), **`self.selected` sigue siendo `None`** hasta que alguien despache `Action::Load(ActionLoad::CatalogWithFilters(None))`.
- **Corrección en el Roadmap:**
  - En `app/src/event_loop.rs`, cuando `profile_catalogs_changed` es `true` y `model.discover.selected.is_none()`, despachar `Action::Load(ActionLoad::CatalogWithFilters(None))`.

### Trampa 3: Por qué se revirtió el filtro de `"metas":[]` en `6635642` (Cuidado con el Paso 3.4 del roadmap anterior)
- **Hecho verificado en el historial (`687db98` vs `6635642`):**
  - En el roadmap anterior puse: *"No guardar en `http_cache` respuestas con `"metas":[]`"*.
  - Revisando por qué `6635642` lo revirtió: muchos catálogos legítimamente devuelven `{"metas":[]}` (por ejemplo, una página final de paginación, o un filtro de género/búsqueda sin resultados). Si `http_cache` borra o rechaza `{"metas":[]}` en cada lectura, o si `run_startup_maintenance` hace un `DELETE FROM http_cache WHERE body LIKE '%"metas":[]%'` escaneando blobs de hasta 8 MB, genera I/O pesado en SQLite.
  - En cambio, lo que **SÍ** estaba mal en `v0.99.7` (`core-env/src/http_cache.rs:107`) es que `MAX_AGE_SECONDS` era **30 días** (`30 * 24 * 60 * 60`) para todos los recursos por igual. Si un catálogo fallaba o devolvía vacío una vez y el servidor no mandaba `ETag`/`Last-Modified`, quedaba cacheado hasta 30 días.
- **Corrección en el Roadmap:**
  - **NO** escanear strings `"metas":[]` con `LIKE` en SQLite ni meter heurísticas frágiles sobre el JSON.
  - En su lugar, aplicar el cambio limpio de `687db98` que se mantuvo en `6635642`: limitar el TTL máximo (`is_expired`) de `"catalog" | "addon_catalog" | "legacy_catalog"` a **6 horas** (`6 * 3600`) en vez de 30 días.

### Trampa 4: `trigger_initial_load` en `app/src/main.rs:1075` corre en un `tokio::spawn` DESPUÉS de `sync_tab_from_model`
- **Hecho verificado en `v0.99.7` (`app/src/main.rs:1069-1075`):**
  ```rust
  if let Ok(initial_tab) = Tab::try_from(navigation.active_tab_index()) {
      sync_tab_from_model(initial_tab, &runtime, &ui, &ui_weak, &navigation);
  }
  ui.set_loading(false);
  callbacks::trigger_initial_load(&runtime);
  ```
  - Mirá el orden en `v0.99.7`:
    1. Primero corre `sync_tab_from_model(Tab::Board)`. En ese momento `model.board.selected` es **`None`** y `model.board.catalogs` está **vacío (`len == 0`)**, porque todavía no se llamó a `trigger_initial_load`!
    2. Por lo tanto, ese primer `sync_tab_from_model` proyecta **0 secciones** en la UI.
    3. Recién después llama a `callbacks::trigger_initial_load(&runtime)`, que encima hace `tokio::spawn` antes de llamar a `rt.dispatch(...)`.
- **Corrección en el Roadmap:**
  1. En `trigger_initial_load(&runtime)`: ejecutar los `runtime.dispatch(...)` **de forma síncrona y directa** (sin `tokio::spawn`), porque `Runtime::dispatch` en `stremio-core` no bloquea en red (solo actualiza los structs en memoria y encola los `EffectFuture` en el executor de Tokio).
  2. En `app/src/main.rs:1069`: llamar a **`callbacks::trigger_initial_load(&runtime)` ANTES de `sync_tab_from_model`**.
  3. **Efecto inmediato:** Cuando corre `sync_tab_from_model(Tab::Board)` en la línea siguiente, `model.board.catalogs` **ya tiene los catálogos creados y en estado `Loading`**, por lo que en el **primerísimo frame** Slint ya recibe todas las filas (`BoardSection`) con sus títulos reales (*Popular - Movie*, *Popular - Series*, etc.) y sus placeholders listos, mientras los 6 requests HTTP ya están viajando en paralelo.

---

## 2. El Flujo Exacto Corregido (De Punta a Punta)

Con las 4 correcciones de arriba, mirá cómo queda la secuencia de arranque al milisegundo:

```text
1. load_startup_storage() hidrata Profile + Library desde Turso.
2. Runtime::new(model, all_effects, 1000) crea el modelo en memoria.
3. callbacks::trigger_initial_load(&runtime) corre SÍNCRONO (antes de sync_tab_from_model):
   3a. dispatch(ActionLoad::CatalogWithFilters(None))          -> Discover empieza a pedir su catálogo 0.
   3b. dispatch(ActionLoad::CatalogsWithExtra(Selected::none)) -> model.board.catalogs se llena con N catálogos (content: None).
   3c. dispatch(ActionCatalogsWithExtra::LoadRange(0..6))      -> Los primeros 6 catálogos pasan a Some(Loading) y disparan sus 6 HTTP GET en paralelo.
   3d. dispatch(ActionLoad::LibraryWithFilters(...))           -> Library queda lista en memoria.
   3e. dispatch(ActionLoad::InstalledAddonsWithFilters(...))   -> Addons instalados listos en memoria.
   3f. dispatch(ActionCtx::PullAddonsFromAPI)                  -> En background verifica si hay addons nuevos en la cuenta.
4. sync_tab_from_model(initial_tab) corre inmediatamente después:
   -> Como 3b y 3c ya ocurrieron síncronamente, el primer render de la ventana YA tiene las secciones del Board en estado Loading y las peticiones HTTP en vuelo.
5. En core-env/http_cache.rs:
   -> Los cache-hits leen de Turso (drenando `rows`) y guardan `accessed_at` en un HashMap en memoria (0 escrituras SQLite en el arranque).
   -> El mantenimiento de DB espera 15s (no 2s), dejando el mutex de Turso 100% libre para los catálogos.
6. A medida que cada catálogo pasa a Ready, `event_loop.rs` proyecta la fila en el Board.
```

---

## 3. Lista Cerrada y Exacta de Cambios sobre `v0.99.7`

### Archivo 1: `app/src/callbacks.rs`
1. En `trigger_initial_load(runtime: &Arc<Runtime<DesktopEnv, AppModel>>)`:
   - Quitar el `tokio::spawn` envolvente y despachar directamente:
     - `Action::Load(ActionLoad::CatalogWithFilters(None))`
     - `Action::Load(ActionLoad::CatalogsWithExtra(Selected { r#type: None, extra: vec![] }))`
     - Despachar en el acto sin hacer lock manual sobre el modelo:
       `Action::CatalogsWithExtra(ActionCatalogsWithExtra::LoadRange(0..crate::models::CATALOG_PRELOAD_ROWS + 1))`
       *(Nota: `CatalogsWithExtra::update` en `stremio-core` acota internamente con `range.end.min(self.catalogs.len())`, por lo que no es necesario inspeccionar `board.catalogs.len()` con locks explícitos).*
     - `Action::Load(ActionLoad::LocalSearch)`
     - `Action::Load(ActionLoad::LibraryWithFilters(...))`
     - `Action::Load(ActionLoad::InstalledAddonsWithFilters(...))`
     - `Action::Ctx(ActionCtx::PullAddonsFromAPI)`
2. Agregar `ui.on_request_reload_current_view` (`F5`) para permitir recarga manual limpia (`Action::Unload` en `Board` + `trigger_initial_load(&runtime)`).

### Archivo 2: `app/src/main.rs`
1. En `finish_startup` (líneas 1069–1076):
   - Invertir el orden para que `callbacks::trigger_initial_load(&runtime);` se ejecute **antes** de `sync_tab_from_model(initial_tab, &runtime, &ui, &ui_weak, &navigation);`.
2. Quitar el timer periódico de 60s de `trim_process_memory(false)` y la llamada a `trim_process_memory(false)` dentro de `on_tab_changed` (que causaba un tirón en el hilo de UI al cambiar de pestaña).

### Archivo 3: `app/src/event_loop.rs`
1. Antes de `board_sync_needed` (línea 504):
   - Si `fields.contains(&AppModelField::Board)` o `profile_catalogs_changed`:
     - Verificar si alguno de los primeros `CATALOG_PRELOAD_ROWS` catálogos en `model.board.catalogs` tiene `first().is_none_or(|p| p.content.is_none())`.
     - Si es así, hacer `if let Some(loader) = crate::models::board::get_catalog_loader() { let _ = loader.try_send(0); }`.
     - Esto cubre el caso en que `PullAddonsFromAPI` o un login posterior agregue catálogos nuevos con `content: None`, **incluso si el usuario está en otro tab**.
2. Si `profile_catalogs_changed && model.discover.selected.is_none()`, encolar `ActionLoad::CatalogWithFilters(None)` para que Discover tampoco quede vacío tras un primer login.

### Archivo 4: `app/src/models/board.rs` y `app/src/models/mod.rs`
1. En `app/src/models/mod.rs`:
   - Hacer `pub(crate) const CATALOG_PRELOAD_ROWS: usize = 5;`.
   - Mantener `visible_catalog_load_range` comprobando **únicamente** `page.content.is_none()` (tal como en `v0.99.7`, sin tocar `Err`).
   - En `fingerprint_catalog_projection`, no hashear `error.to_string()` dentro de `Some(Loadable::Err(_))`.
   - En `refresh_cached_media_images`, retornar `0` temprano si `urls.is_empty()` o si ninguna URL del lote está en caché.
2. En `app/src/models/board.rs`:
   - Exponer `pub(crate) fn get_catalog_loader() -> Option<tokio::sync::mpsc::Sender<usize>>`.
   - En `board::sync`, revisar `take(crate::models::CATALOG_PRELOAD_ROWS)` con `p.content.is_none()`.

### Archivo 5: `core-env/src/http_cache.rs`, `core-env/src/lib.rs` y `app/src/db.rs`
1. **Drenado de cursores Turso:** agregar `while let Ok(Some(_)) = rows.next().await {}` antes de soltar `rows` en `http_cache::read_entry`, `core_env::load_profile_scope`, `core_env::get_storage` y `db::get_setting`.
2. **Batching de `accessed_at` en memoria (`http_cache.rs`):** reemplazar el `spawn_on_runtime` por cada lectura por `PENDING_ACCESS_TOUCHES` con flush cada 30s y en `shutdown_database()`.
3. **TTL y Retries en `http_cache.rs`:**
   - Expiración de catálogos a las 6 horas (`6 * 3600`) en vez de 30 días.
   - Extender `send_request_with_retry` a `Meta | LegacyMeta | AddonCatalog | Manifest`.
4. **Mantenimiento diferido en `app/src/db.rs`:** cambiar el sleep inicial de `run_startup_maintenance` de `2` a `15` segundos.

---

## 4. Veredicto de la Segunda Revisión

Con estas 4 correcciones adicionales:
1. **No hay huecos en el arranque:** `trigger_initial_load` corre **antes** del primer `sync_tab_from_model` y despacha `ActionLoad::CatalogsWithExtra` + `LoadRange(0..6)` en forma **síncrona**, por lo que el primer frame de la ventana ya tiene las filas del Board creadas y las 6 descargas HTTP en curso.
2. **No hay desincronización de addons:** `PullAddonsFromAPI` se despacha al iniciar y, si trae cambios, el event loop dispara `LoadRange` para los catálogos nuevos sin importar en qué pestaña esté el usuario.
3. **No hay contención en SQLite/Turso:** 0 escrituras de `accessed_at` durante el arranque, mantenimiento pospuesto a los 15s y cursores drenados.
4. **Es imposible entrar en loop de CPU/red:** `LoadRange` solo se pide para `content.is_none()`, que pasa inmediatamente a `Some(Loading)`.

El archivo `/home/kes/Documentos/Programando/stremio-native/Roadmap_Arranque_Catalogos_v0.99.7.md` ya tiene volcada esta versión corregida. ¿Avanzamos con la implementación de estos 5 puntos?