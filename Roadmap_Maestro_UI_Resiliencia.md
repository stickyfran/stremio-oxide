# Roadmap Maestro: Rescate de UI/UX + Blindaje de Fiabilidad + Erradicación del Stutter

**Proyecto:** Stremio Oxide / Native
**HEAD auditado:** `519a3e9` — *fix(board): eliminate destructive watchdog and fix catalog unloaded condition*
**`origin/main`:** `6635642`
**Versión en `app/Cargo.toml`:** `0.99.82`
**Fecha de auditoría:** 2026-11-02
**Estado:** documento de planificación. **No se implementó ningún cambio de código.**
**Documentos fuente:**
- `Details123.md` — versión ampliada y corregida del PDF `Details123.pdf` (UI/UX).
- `RoadmapConsistency.md` — roadmap de fiabilidad/DoH/watchdogs/toasts que el usuario no encontraba.
- `Roadmap123123.md` — antecedente de rendimiento, SQLite y arranque.
- `AuditoriaDeep.md`, `Roadmap_Next*.md`, `Roadmap_UI_UX_Fixes.md`, `UiNext.md` — contexto histórico.

---

## 0. Resumen ejecutivo

Los últimos cambios fallaron por **tres problemas independientes que se retroalimentan**:

1. **El WIP de UI no implementa lo que `Details123.md` pide.** Toca 8 archivos `.slint`, pero no mueve el control de preview al extremo superior derecho, no unifica la X con `preview-enabled`, no elimina el gutter permanente del drawer, no corrige el desfase de 6u cuando la Sidebar está oculta y no arregla el tray real de Linux/Windows. Varias animaciones se declararon, pero están desconectadas del estado real o generan reflow.
2. **Los intentos de fiabilidad introdujeron un bucle de recarga de catálogos.** El Board reencola `LoadRange` cada vez que ve un catálogo `None` **o `Err`**, y el loader también trata `Err` como “no cargado”. Resultado: un catálogo que falla entra en ciclo `Err → Loading → Err`, reescribe el modelo y vuelve a disparar `sync`/layout.
3. **El stutter tiene causas medibles en el hilo de UI.** Se agregaron `Timer` de 16 ms por cada skeleton y por cada fila de carrusel, se animan propiedades que afectan layout (`y`, `width`, `x`), y las funciones `sync` de Library/Discover mantienen el lock de lectura del modelo mientras hacen trabajo O(n). A eso se suman `refresh_cached_media_images` recorriendo toda la pestaña activa cada ~25 ms y `mi_collect` ejecutándose en el hilo de UI al cambiar de pestaña.

**Conclusión:** no hay que “agregar más animaciones” ni “más retries”. Primero hay que **detener la hemorragia (P0)**: neutralizar el bucle de catálogos, eliminar los timers por instancia y sacar el trabajo pesado del hilo de UI. Recién después se corrige la geometría, el drawer y el tray, con criterios verificables.

> **Regla de oro de este roadmap:** ninguna fase se declara “completada” por haber escrito el código. Se completa cuando pasa su matriz de aceptación y las métricas de la sección 9.

---

## 1. Estado real del repositorio (evidencia)

### 1.1 Working tree

```
M app/ui/app.slint
M app/ui/assets/images/tray-icon.svg
M app/ui/components/feedback.slint
M app/ui/components/media_card.slint
M app/ui/components/media_carousel.slint
M app/ui/components/preview_drawer.slint
M app/ui/pages/discover.slint
M app/ui/pages/library.slint
?? Details123.md
```

**Hechos importantes:**

- Solo hay cambios de UI. **No hay cambios en Rust**, ni en `app/ui/pages/board.slint`, ni en `app/assets/tray.ico`, ni en `scripts/gen-tray-ico.ps1`.
- Por lo tanto, cualquier requisito que dependa del backend (tray Windows, DoH, DB, watchdogs, toasts) **es imposible que esté resuelto** en este working tree.
- `Details123.md` está sin trackear: es la fuente de diseño, no código.
- El build release anterior se hizo sobre `f4c4541`; **no refleja `519a3e9` ni el WIP actual**.

### 1.2 Qué se auditó

- Commits: `7ab2ed6`, `ac25630`, `6635642`, `519a3e9`, `34b75f6`, `ee3ba4c`, `ae80f02`.
- UI: `app.slint`, `library.slint`, `discover.slint`, `preview_drawer.slint`, `feedback.slint`, `media_card.slint`, `media_carousel.slint`.
- Backend: `app/src/models/board.rs`, `app/src/models/mod.rs`, `app/src/models/library.rs`, `app/src/models/discover.rs`, `app/src/event_loop.rs`, `app/src/main.rs`, `core-env/src/lib.rs`, `core-env/src/dns.rs`, `app/src/tray/*.rs`, `vendor/i-slint-core/items/system_tray/ksni.rs`.

---

## 2. Matriz “prometido vs implementado vs funciona”

Leyenda: ✅ cumple · 🟡 parcial · ❌ no cumple · ⛔ imposible en el diff actual.

### 2.1 UI/UX (`Details123.md`)

| Requisito prometido | Implementado en WIP | ¿Funciona? | Evidencia |
|---|---|---|---|
| Buscador alineado con filtros y primera card | 🟡 movió padding a 1.5u | ❌ con Sidebar oculta queda 6u corrido | `app.slint:1369-1378`, `app.slint:1676-1679` |
| Filtros y cuadrícula alineados al píxel | ❌ | ❌ la card tiene offset interno de 1u | `media_card.slint:184-188` |
| Fila de filtros sin desborde | ❌ | ❌ 6 sorts de 7u desbordan a 1366px | `library.slint` fila de filtros |
| Botón “Show Preview” en extremo superior derecho | ❌ | ❌ sigue en la fila de filtros | `library.slint` toolbar |
| X del drawer desactiva `preview-enabled` | ❌ | ❌ solo llama `close-drawer` | `preview_drawer.slint`, `discover.slint:394` |
| Drawer cierra con animación y el grid recupera ancho a la vez | ❌ | ❌ grid salta primero, drawer tarda 200 ms → snap | `library.slint:125`, `preview_drawer.slint:89-122` |
| Sin gutter permanente si el preview está cerrado | ❌ | ❌ reserva 29u mientras `preview-enabled` sea true (default true) | `library.slint:35,125` |
| Skeletons con fade-in escalonado y sin repintado infinito | ❌ | ❌ `Timer` de 16 ms por bloque (WIP) / shimmer infinito (HEAD) | `feedback.slint:8-21` |
| Transición de filas/carruseles sin reflow | ❌ | ❌ anima `y` y `opacity` con timer de 16 ms por fila | `media_carousel.slint:161-177` |
| Tray monocromático visible en tema claro y oscuro | 🟡 cambió el SVG | ❌ Linux sigue con `icon_name` inexistente; Windows sigue con `.ico` blanco puro | `ksni.rs:44-67`, `windows.rs:215-237` |
| i18n de “Show/Hide Preview” | ❌ | ❌ cadenas hardcodeadas | `library.slint` |

### 2.2 Fiabilidad (`RoadmapConsistency.md`)

| Requisito prometido | Implementado | ¿Funciona? | Evidencia |
|---|---|---|---|
| DoH mediante `reqwest::dns::Resolve` | ❌ es un fallback que reescribe la URL a una IP | 🟡 solo en algunos fallos; cache nunca se lee | `core-env/src/lib.rs:333-350`, `core-env/src/dns.rs` |
| Cache DNS consultada antes de resolver | ❌ `DNS_CACHE.insert` sin `get` | ❌ | `core-env/src/lib.rs:27,338` |
| DoH con validación TLS correcta | ❌ `danger_accept_invalid_certs(true)` | ⚠️ riesgo de seguridad | `core-env/src/lib.rs:30-38` |
| Toasts deduplicados (“bloqueo evitado”, “DPI detectado”) | ❌ no existe `toast_manager.rs` ni `toast.slint` | ❌ | solo `ToastItem` en `types.slint:26` |
| Watchdog de arranque no destructivo | ✅ eliminado en `519a3e9` | ✅ | `main.rs` |
| Watchdog de salida y del servidor | ✅ existen | 🟡 revisar interacción con UI | `main.rs:337,381,629-716` |
| DB sin contención | 🟡 se redujo, pero sigue un `AsyncMutex` global | ⚠️ puede bloquear lecturas | `core-env/src/lib.rs:404-405` |
| Catálogos sin carrera de arranque | 🟡 se agregó loader visible | ❌ nuevo bucle `Err → Loading → Err` | `board.rs:565-577`, `mod.rs:253` |

---

## 3. Diagnóstico del stutter (causas raíz)

> Clasificación: **P0** = produce stutter severo o bucle; **P1** = agrava de forma medible; **P2** = riesgo/ruido a vigilar.
> “Hecho” = verificado en código; “Hipótesis” = requiere perfilado para confirmar magnitud.

### P0-A. Bucle de recarga infinita de catálogos del Board

**Hecho (código):**

- `board.rs:565-577`: después de cada `sync`, si **cualquiera de los 3 primeros catálogos** está `None` **o `Err`**, hace `loader.try_send(0)`.
- `models/mod.rs:250-254`: el loader considera “no cargado” a `None` **y a `Err`**, y despacha `LoadRange`.
- `board.rs:379-380`: el `sync` se gatilla por un fingerprint que incluye la proyección del catálogo; `models/mod.rs:309-312` incluye el texto del error en el fingerprint.

**Mecanismo del stutter:**

```
catálogo falla → Err
  → board.sync detecta Err → try_send(0)
    → loader ve Err como “unloaded” → LoadRange
      → estado pasa a Loading → cambia fingerprint → sync
        → vuelve a fallar → Err → try_send(0) → …
```

Cada vuelta reescribe propiedades Slint, invalida filas y puede re-disparar layout. Sin backoff, esto es un ciclo caliente.

**Causa raíz:** se confundió “no cargado” con “falló”. Un error es un **estado terminal** hasta que haya una acción explícita (retry del usuario, cambio de addon, reconexión), no una invitación a recargar.

**Remediación:**

1. Definir una máquina de estados explícita por catálogo: `Idle → Loading → Ready | Failed { attempts, last_error, next_retry_at }`.
2. `Err` **no** debe contar como “unloaded”.
3. Retry con backoff exponencial + jitter (p. ej. 500 ms, 1 s, 2 s, 4 s, máx. 30 s y máx. 5 intentos), y solo para errores transitorios.
4. Un solo `LoadRange` en vuelo por catálogo (`in_flight` flag).
5. El fingerprint no debe incluir el texto del error; solo el estado estable y el contenido.
6. Retry manual visible (botón “Reintentar”) en la sección fallida.

### P0-B. Timers de 16 ms por cada skeleton y por cada fila

**Hecho (working tree):**

- `feedback.slint:8-13`: cada `SkeletonBlock` crea su propio `Timer { interval: 16ms }`.
- `media_carousel.slint:161-177`: cada fila crea otro `Timer { interval: 16ms }` y anima `opacity, y`.
- `feedback.slint` en HEAD usa shimmer infinito (`iteration-count: -1`), que también repinta para siempre.

**Por qué es severo:** una pantalla de carga puede tener 50–200 skeletons. Eso son 50–200 timers independientes despertando el event loop ~60 veces por segundo, cada uno invalidando su nodo. Es exactamente el patrón que `Details123.md` prohíbe (“cero ciclos innecesarios de CPU/GPU”).

**Remediación:**

1. **Cero timers por instancia.** Un único driver de animación global (un `Timer` en el contenedor de la página, o animación declarativa de Slint) que actualice una propiedad compartida.
2. Preferir `animate opacity` declarativo con `iteration-count` finito o un shimmer **puramente visual** (gradiente que se mueve) sin tocar layout.
3. Respetar `prefers-reduced-motion`: si está activo, sin shimmer.
4. Detener la animación cuando la página no es visible (`visible: false`) o la app está minimizada.
5. Nunca animar `y`, `height`, `width` en listas virtualizadas; solo `opacity` y, si acaso, `transform`/`scale`.
6. Condición estricta del driver: `any_catalog_loading && window_visible && !window_minimized && !reduced_motion`. Al pasar a `Ready`/`Failed`, los wakeups deben caer a 0.

### P0-C. Animaciones que provocan reflow en `ListView`

**Hecho:**

- `media_carousel.slint:87`: `animate y`.
- `media_carousel.slint:177`: `animate opacity, y`.
- `preview_drawer.slint:121-122`: `animate x` + `animate opacity`; el `width` cambia por estado booleano, no animado.
- `library.slint:125`: `effective-drawer-width` depende de `preview-enabled`, no de `is-open`.

**Mecanismo:** animar `y` dentro de un `ListView` obliga a recalcular posiciones y puede invalidar el viewport completo. El drawer cambia el ancho del grid de golpe mientras su propio `width` tarda 200 ms → snap visible.

**Remediación:**

1. Animar solo propiedades compositivas (`opacity`, `transform`).
2. **No interpolar `grid-width` por frame.** Es la trampa de la solución ingenua: si el ancho del contenedor cambia en cada tick, el `ListView` recalcula todas las filas visibles a 60/120 Hz durante 200 ms y reaparece el stutter que se quiere eliminar. El drawer debe resolverse con overlay o con resize atómico + crossfade (ver §6.1).
3. `clip: true` en el drawer y `overflow` controlado para que el slide no se corte.
4. Nada de `Timer` manual para cerrar: usar `animate` con estado.

### P1-A. `sync` de Library/Discover mantiene el lock del modelo durante O(n)

**Hecho:**

- `library.rs:418`: `let model_guard = runtime.model().ok();` y se usa hasta ~536 para fingerprinting y mapeo.
- `discover.rs:542`: mismo patrón hasta ~677.

**Mecanismo:** si el hilo de red/DB quiere escribir el modelo (dispatch), espera a que termine todo el mapeo de la pestaña. En catálogos grandes, eso bloquea y produce picos.

**Remediación:**

1. Tomar el lock solo para **clonar** los submodelos necesarios (ya se hace parcialmente en `event_loop.rs:620+`).
2. Soltar el guard antes de fingerprinting/mapeo.
3. Mover la construcción de filas a estructuras locales y aplicar a Slint al final, en un solo batch.
4. Medir con spans `tracing` la duración de `sync` (p50/p95/p99).

### P1-B. `refresh_cached_media_images` recorre toda la pestaña cada ~25 ms

**Hecho:**

- `models/mod.rs:535-615`: recorre Board/Discover/Library/Addons/Calendar/Search, hace `row_data`, clona cards y llama `set_row_data`.

**Mecanismo:** trabajo O(filas × cards) periódico en el hilo de UI, incluso cuando no llegó ninguna imagen nueva.

**Remediación:**

1. Solo parchear las URLs que efectivamente se decodificaron en ese tick (ya recibe `urls`, pero recorre todo igual).
2. Índice inverso `url → [(modelo, fila, col)]` para updates O(k).
3. Coalescer en un frame (batch) y no más de una vez cada 100–150 ms.
4. Cancelar el refresco si la pestaña cambió.

### P1-C. `mi_collect` en el hilo de UI

**Hecho:**

- `event_loop.rs:1213-1218`: `trim_process_memory(force)` llama `libmimalloc_sys::mi_collect(force)`.
- Invocado en `main.rs:747` (cada 60 s), `main.rs:890` (cambio de pestaña), `main.rs:1087` (single-shot de arranque), `shortcuts.rs:762` y `mpv_integration.rs:5335` (ambos con `force = true`).

**Mecanismo (corrección importante):** `libmimalloc-sys 0.1.49` compila mimalloc **v3** salvo que se active la feature `v2`; la app no la activa. En v3, `mi_collect(force)` llama `_mi_theap_default()` y recolecta **el theap del hilo actual** y las arenas globales, pero **no recorre los heaps activos de otros hilos**. Por lo tanto:

- Mover `mi_collect` a un `std::thread::spawn` o a un worker de Tokio **no reclaima la memoria del hilo de UI**; solo recolecta el heap vacío del hilo nuevo y las arenas.
- El `tokio::spawn` de `main.rs:747` ya corre en un worker de Tokio: ese `mi_collect(false)` periódico **nunca recolecta el heap de UI**. Es, en la práctica, casi un no-op para el objetivo buscado.
- El que sí recolecta el heap de UI es `main.rs:890` (callback de cambio de pestaña), y es justamente el que produce el pico.

**Remediación:**

1. **Quitar `trim_process_memory(false)` del camino de navegación** (`main.rs:890`). No debe ejecutarse al cambiar de pestaña.
2. Si se necesita devolver RSS tras picos grandes, ejecutar `mi_collect` **en el hilo de UI pero solo cuando la ventana esté oculta/minimizada/en tray**, donde una pausa de 10–50 ms es invisible. Alternativa: tras N segundos de inactividad real, con presupuesto medido.
3. Configurar el decay del allocator al inicio del proceso en vez de forzar colectas manuales: `mi_option_purge_delay` (default 10 ms; `-1` desactiva, `0` purga inmediato) y `mi_option_purge_decommits`. Los alias legacy son `mi_option_reset_delay` / `mi_option_reset_decommits`; `mi_option_deprecated_page_reset` está deprecado. Validar el efecto en RSS antes de fijar valores.
4. Mantener `trim_process_memory(true)` solo en minimize/tray y al descargar el reproductor, verificando que corra en el hilo de UI (es donde está el heap que se quiere recolectar).
5. Medir la duración real de `mi_collect` en el hilo de UI con un span; si supera 4 ms, no puede quedar en ningún camino interactivo.

> **No hacer:** “moverlo a background” esperando que libere el heap de UI. Esa suposición es falsa con mimalloc v3.

### P1-D. Clonado de submodelos en el hilo de UI

**Hecho:** `event_loop.rs:620+` clona `continue_watching`, `board`, `addons`, `discover`, etc. según necesidad.

**Mecanismo:** clones profundos de estructuras grandes dentro del lock.

**Remediación:** mover la proyección a un worker y entregar un snapshot inmutable; o usar `Arc` cuando el modelo lo permita. Como mínimo, medir tamaño y frecuencia.

### P2-A. Un único `AsyncMutex` para toda la conexión Turso

**Hecho:** `core-env/src/lib.rs:404-405`.

**Mecanismo:** mantenimiento de DB/cache puede serializar lecturas que la UI necesita.

**Remediación:** verificar WAL primero y hacer un spike de concurrencia con Turso 0.7.2 antes de tocar el mutex. Si el spike da positivo: conexión de escritura detrás de MPSC + conexiones de solo lectura (`db.connect()`, no `Connection::clone()`). Si no: mantener el mutex y optimizar batching/mantenimiento. Ver §10.1.

### P2-B. Cadenas de animación en listas virtualizadas

**Hecho:** `media_carousel.slint:50,86` anima `opacity`; 177 anima `y`.

**Remediación:** ver P0-C; además, limitar la animación de entrada a las primeras N filas visibles, no a todo el modelo.

### Resumen de prioridad del stutter

| Prioridad | Causa | Impacto esperado | Esfuerzo |
|---|---|---|---|
| P0-A | Bucle `Err → Loading → Err` | Muy alto, continuo | 2–3 días |
| P0-B | Timers 16 ms por skeleton/fila | Muy alto en carga/scroll | 1–1.5 días |
| P0-C | Animación de `y`/`width` en listas | Alto, picos y snap | 1–2 días |
| P1-A | Lock del modelo durante O(n) | Alto en catálogos grandes | 2–3 días |
| P1-B | Refresco O(n) cada 25 ms | Medio-alto | 1–2 días |
| P1-C | `mi_collect` en UI thread | Medio, picos al cambiar pestaña | 0.5 día |
| P1-D | Clones de submodelos | Medio | 1–2 días |
| P2-A | Mutex global Turso | Medio, intermitente | 2–3 días |
| P2-B | Animaciones en listas | Bajo-medio | incluido en P0-C |

---

## 3.5. Revisión de la auditoría externa (veredicto y ajustes aplicados)

La auditoría externa señaló cinco trampas. Se revisaron contra el código real: **cuatro son correctas (una con matices) y una ya estaba parcialmente resuelta**. Esto es lo que cambió en este documento:

| # | Observación | Veredicto | Acción en este roadmap |
|---|---|---|---|
| 1 | Animar `grid-width` con `drawer-progress` provoca reflow por frame | ✅ **Correcta** | Se eliminó la interpolación de `grid-width`. §6.1 ahora usa overlay o resize atómico + crossfade |
| 2 | `mi_collect` en background no reclaima el heap de UI | ✅ **Correcta con matiz** | Se corrigió P1-C: mimalloc v3 es thread-local; se documentó que el timer de 60 s ya es un no-op para UI |
| 3 | Los objetivos de frame time deben depender del refresh del monitor | ✅ **Correcta** | §11.1 se reescribió con presupuesto `1000/Hz` y separación UI-thread vs render |
| 4 | El driver de skeletons debe apagarse al terminar la carga | ✅ **Correcta** | Se agregó la condición estricta en §7.1 y un criterio de wakeups = 0 |
| 5 | Verificar WAL antes de armar un pool de conexiones | 🟡 **Parcialmente correcta** | WAL y `synchronous=NORMAL` ya están en `app/src/db.rs:42-62`; se refuerza el chequeo y se exige benchmark antes del pool (§10.1) |

### 3.5.1. Punto 1 — drawer y `grid-width`

La crítica es válida. La propuesta original (`grid-width = total - 29u * progress`) habría cambiado el layout de todas las filas visibles en cada frame de los 200 ms de transición, con riesgo de saltos de columna. **Se descarta.** El diseño corregido está en §6.1:

- **Overlay** (recomendado en anchos chicos): el grid nunca cambia de ancho; el drawer se desliza encima con `x` + `opacity` y un scrim. Cero reflow.
- **Push atómico** (anchos grandes): `grid-width` cambia **una sola vez** al inicio de la apertura o al final del cierre, con un crossfade de `opacity` de 100–120 ms; el drawer anima solo `x`/`opacity`.
- Prohibido interpolar `width` o `grid-width` por frame en cualquier lista virtualizada.

### 3.5.2. Punto 2 — mimalloc es thread-local

La crítica es correcta en lo esencial. Verificado en el código vendorizado de `libmimalloc-sys 0.1.49`:

- La app no activa la feature `v2`, así que se compila **mimalloc v3** (`build.rs:8-12`).
- `mi_collect(force)` → `mi_theap_collect(_mi_theap_default(), force)` → `mi_theap_collect_ex(...)`; recolecta el theap del hilo actual y las arenas globales, **no los heaps activos de otros hilos** (`c_src/mimalloc/v3/src/theap.c:116-152`).
- Por eso el `tokio::spawn` de `main.rs:747` (cada 60 s) no recolecta el heap de UI: ya es un no-op para ese objetivo. El que sí lo hace es `main.rs:890`, en el cambio de pestaña, y ese es el pico a eliminar.

**Matiz:** `mi_collect(true)` sí fuerza la recolección de arenas globales y páginas retiradas, pero no compacta las páginas activas del hilo de UI. Mover la llamada a background no resuelve el problema; solo lo esconde.

### 3.5.3. Punto 3 — FPS/Hz dependen del usuario

Correcto. Fijar “16.7 ms” como objetivo universal era un error: a 120 Hz el budget es 8.33 ms; a 144 Hz, 6.94 ms; a 240 Hz, 4.17 ms. Se separó el presupuesto en dos capas:

1. **Trabajo del hilo de UI** (callbacks Rust/Slint): `< 4 ms p95`, `< 8 ms p99`, independiente del monitor.
2. **Cadencia de render**: atada al VSync real del monitor, medida con `refresh_rate_millihertz()` y herramientas de presentación (MangoHud/PresentMon), no asumida a 60 Hz.

El mínimo garantizado se valida a 60 Hz; 120/144/240 Hz son presupuesto de no-regresión en hardware que los soporte.

### 3.5.4. Punto 4 — apagado del driver de skeletons

Correcto y crítico. El driver global debe cumplir:

```
running = any_catalog_loading
       && window_visible
       && !window_minimized
       && !reduced_motion
```

Cuando el último catálogo pasa a `Ready` o `Failed`, `any_catalog_loading` debe ser `false` y los wakeups de animación deben caer **exactamente a 0**. Un timer global que sigue corriendo “por las dudas” reintroduce el problema de P0-B con otra forma.

### 3.5.5. Punto 5 — WAL ya está, pero hay que verificarlo

Parcialmente correcta. El código actual **ya abre WAL**:

- `app/src/db.rs:42-54`: `PRAGMA journal_mode = WAL` por query, con warning si no queda en `wal`.
- `app/src/db.rs:58-62`: `synchronous = NORMAL`, `foreign_keys = ON`, `temp_store = MEMORY`, `cache_size = -4000`, `mmap_size = 30000000`.
- `core-env/src/lib.rs:608-610`: `install_database` reutiliza la misma `turso::Database`; `get_db_conn` (`:636-668`) abre conexiones nuevas con `synchronous = NORMAL`, `busy_timeout = 1000`, etc. WAL es persistente a nivel de base, así que las conexiones nuevas lo heredan.

**Pero:** el fallo de WAL solo emite un `warn` (no aborta), `core-env` no repite/verifica `journal_mode`, y no hay benchmark que demuestre que Turso 0.7.2 permite lecturas concurrentes reales con WAL. Por eso §10.1 exige:

- Hard-fail si `journal_mode != wal` al inicializar.
- Verificación idempotente en `core-env` antes de crear el pool.
- **Spike/benchmark previo**: conexiones separadas con `db.connect()` (no `Connection::clone()`, que comparte estado de transacción), `PRAGMA query_only = ON` para lectores, y una conexión de escritura detrás de un MPSC con batching.
- Test de contención con 100 lecturas + 1 mantenimiento **antes** de eliminar el `AsyncMutex`.

---

## 4. Fase 0 — “Stop the bleeding” (P0, obligatoria antes de todo)

**Objetivo:** dejar la app estable y medible sin agregar features.

### 0.1 Congelar y aislar el WIP

1. Crear una rama `wip/ui-attempt-1` y commitear el working tree actual **tal cual** (incluido `Details123.md`).
2. Crear `stash`/tag de respaldo: `git tag wip-ui-attempt-1`.
3. No borrar nada: el WIP es evidencia y puede contener piezas reutilizables (p. ej. el SVG del tray).
4. Trabajar las correcciones en commits pequeños y separados por tema.

**Criterio de aceptación:** el estado actual es recuperable con un solo comando; el árbol limpio compila.

### 0.2 Desactivar el bucle de catálogos

- Cambiar la condición de `board.rs:566-574` para que **solo** considere `None`/`Loading`, nunca `Err`.
- Añadir `Failed` como estado terminal y un contador de intentos.
- Añadir backoff con jitter.
- Añadir un test unitario que simule 10 fallos y verifique que se despachan como máximo N `LoadRange` (no infinitos).

**Criterio de aceptación:** con un addon que falla siempre, el número de `LoadRange` en 60 s es ≤ 5 y el CPU idle vuelve a < 2 %.

### 0.3 Neutralizar timers por instancia

- Reemplazar los `Timer` de `feedback.slint` y `media_carousel.slint` por una animación declarativa única o un driver global.
- En HEAD, quitar `iteration-count: -1` del shimmer o hacerlo finito.
- Respetar `prefers-reduced-motion`.
- Condición del driver: `any_catalog_loading && window_visible && !window_minimized && !reduced_motion`. Al terminar la carga, wakeups = 0.

**Criterio de aceptación:** con 200 skeletons visibles, el número de timers activos es 1 (o 0) y el CPU durante la carga baja ≥ 50 % respecto del WIP; con todo `Ready`, no queda ningún timer de animación corriendo.

### 0.4 Sacar trabajo pesado del hilo de UI

- **Quitar `trim_process_memory(false)` del cambio de pestaña** (`main.rs:890`). No mover `mi_collect` a background esperando que libere el heap de UI: en mimalloc v3 es thread-local. Dejarlo solo en minimize/tray/oculto, sobre el hilo de UI, o configurar `mi_option_purge_delay` al inicio.
- `refresh_cached_media_images` solo para URLs nuevas y con throttle.
- `sync` de Library/Discover: soltar el lock antes del mapeo.

**Criterio de aceptación:** cambio de pestaña con 500 cards: p95 de frame ≤ `1000/refresh_hz` y sin pico > 100 ms; `mi_collect` no aparece en la traza de navegación.

### 0.5 Instrumentación mínima

Agregar spans/contadores (feature `profiling` o `tracing`):

- `board.sync.duration`, `catalog.loadrange.count`, `catalog.state`.
- `library.sync.duration`, `discover.sync.duration`.
- `refresh_cached_media_images.duration`, `updated_count`.
- `trim_process_memory.duration` y `trim_process_memory.thread`.
- `ui.frame_time`, `ui.refresh_hz`, `ui.dropped_frames`.
- `ui.animation.wakeups`, `ui.grid_width.changes`.
- `tray.icon.state` (Linux/Windows).

**Criterio de aceptación:** existe un comando/documento para capturar 60 s de métricas antes y después.

**Estimación Fase 0:** 3–4 días.

---

## 5. Fase 1 — Alineación geométrica (buscador, filtros, cuadrícula)

**Objetivo:** que buscador, filtros y primera card compartan el mismo eje X en todos los presets y resoluciones, sin reflow.

### 5.1 Definir una única variable de layout

Hoy conviven:

- `Theme.vertical-nav-bar-size = 6u` (`theme.slint:81`, `app.slint:1353`).
- Sidebar visible solo en `Side Rail`/`Classic Home` (`app.slint:1676-1679`).
- Buscador con `padding-left: 1.5u` (`app.slint:1369-1375`) y ancho fijo `28u` (`app.slint:1377-1378`).
- Contenido de página con padding `1.5u`.
- Card con offset interno de `1u` (`media_card.slint:184-188`).

**Solución:**

1. Introducir `Theme.content-left-inset` calculado:
   - `sidebar_visible ? Theme.vertical-nav-bar-size : 0px`.
2. El navbar y las páginas consumen **la misma** variable, no constantes separadas.
3. El padding de contenido debe ser el mismo en navbar y páginas (`1.5u`).
4. Corregir el offset de la card: el póster debe empezar exactamente donde empieza el filtro. Si la card necesita margen para el borde de selección, usar `padding` externo compensado, no un `x` interno que desplace el póster.
5. `SearchBox` deja de ser `alignment: center` global; se ancla al mismo inset que el contenido.

### 5.2 Fila de filtros sin desborde

- Altura actual `5.75u` con `padding: 1.5u` → caja útil `2.75u`.
- `SelectField` pide `15u` y los pills `2.75u`/`3.25u`; seis sorts de `7u` desbordan a 1366px.
- **Solución:** layout responsive:
  - Filtros principales en una fila.
  - Sorts en un `SelectField` o menú overflow cuando no entren.
  - Nunca permitir que la fila cambie de alto al abrir el drawer.
- Medir con `min-width` y `preferred-width`, no con `horizontal-stretch` ciego.

### 5.3 Matriz de alineación

| Resolución | Sidebar visible | Sidebar oculta |
|---|---|---|
| 1280×720 | eje único | eje único |
| 1366×768 | eje único, sin overflow | eje único, sin overflow |
| 1920×1080 | eje único | eje único |
| Drawer abierto | sin reflow del eje | sin reflow del eje |

**Criterio de aceptación:** diferencia ≤ 1 px entre `search.x`, `filters.x` y `first_card_poster.x` en las 8 combinaciones. Sin scroll horizontal en la fila de filtros.

**Estimación Fase 1:** 1.5–2 días.

---

## 6. Fase 2 — Preview Drawer (control, ciclo, anti-snap)

**Objetivo:** un solo control coherente, sin gutter permanente, sin salto del grid.

### 6.1 Estado único

Hoy:

- `library.slint:35`: `preview-enabled: true` por defecto.
- `library.slint:125`: `effective-drawer-width = preview-enabled ? 29u : 0`.
- `preview_drawer.slint:99-122`: `is-open` controla `x`/`opacity`, pero `width` queda en `29u` durante 200 ms por el `closing` timer.

**Solución:**

1. Un solo estado: `preview-mode: Disabled | EnabledClosed | EnabledOpen`.
2. `EnabledClosed` **no reserva ancho** (0u). `Disabled` tampoco. Solo `EnabledOpen` reserva `29u`.
3. **Regla dura: `grid-width` cambia como máximo una vez por transición, nunca por frame.** `drawer-progress` (0→1) se usa únicamente para `drawer.x` y `drawer.opacity` dentro de un contenedor fijo con `clip: true`. No se interpola `width` ni `grid-width`.
4. Elegir el modo según el ancho disponible:
   - **Modo overlay (recomendado < 1600 px):** el drawer se superpone al grid; `grid-width` **no cambia nunca**. Se animan `x`, `opacity` y un scrim sutil. Cero reflow, cero salto de columnas.
   - **Modo push (recomendado ≥ 1600 px):** `grid-width` cambia en **un solo paso** al inicio de la apertura o al final del cierre. Para ocultar el salto de columna, se hace un crossfade de `opacity` del grid de 100–120 ms. El drawer anima solo `x`/`opacity`. La columna nueva aparece una sola vez, no a mitad de la animación.
5. Si se quiere un efecto de “crecimiento”, usar `scale`/`transform` sobre el contenedor con `clip`, nunca `width` de layout.
6. `clip: true` en el drawer y en el contenedor del grid.
7. Sin `Timer` manual de cierre: `preview-mode` es la única fuente de verdad y la animación se deriva de él.

> Esta sección reemplaza la propuesta original de `grid-width = total - 29u * progress`, que la auditoría externa marcó correctamente como reflow por frame. La decisión overlay vs push debe quedar detrás de un flag (`ui.preview-push`) para poder A/B medir.

### 6.2 Botón y X

1. Mover el toggle al extremo superior derecho de la página, alineado con el borde superior del contenido.
2. Texto i18n:
   - `@tr("PREVIEW_SHOW" => "Mostrar vista previa")`
   - `@tr("PREVIEW_HIDE" => "Ocultar vista previa")`
3. La X del drawer:
   - Cierra y pone `preview-enabled = false`.
   - El botón superior pasa a “Mostrar vista previa”.
   - Reabrir en un clic restaura `EnabledOpen` (no solo `is-open`).
4. Igual comportamiento en Library y Discover; hoy Discover no desactiva `preview-enabled`.

### 6.3 Criterios de aceptación

- Abrir/cerrar: una sola animación de 200 ms, sin snap ni salto de columna a mitad.
- **`grid-width` cambia como máximo 1 vez por transición** (contador de layouts = 1), nunca por frame.
- **El número de columnas no cambia durante la animación**; si cambia, ocurre en el paso atómico y queda oculto por el crossfade.
- Con preview cerrado: ancho del grid = ancho completo, sin gutter.
- La X y el toggle reflejan el mismo estado.
- Cambiar de pestaña conserva el estado de preview por página.
- `prefers-reduced-motion`: transición instantánea.
- En modo overlay: 0 cambios de `grid-width` durante toda la interacción.

**Estimación Fase 2:** 2–2.5 días (incluye el flag y la medición A/B overlay vs push).

---

## 7. Fase 3 — Animaciones de carga y transiciones (sin stutter)

**Objetivo:** percepción de fluidez con costo constante y bajo.

### 7.1 Skeletons

- Un solo driver de shimmer por página.
- `opacity`/gradiente, nunca `width`/`height`/`y`.
- Número de skeletons acotado (máx. N visibles + buffer).
- Fade-in escalonado **solo** en la primera aparición; no reanimar al reciclar.
- Respetar `prefers-reduced-motion`.
- **Condición estricta del driver global:**

  ```
  running = any_catalog_loading
         && window_visible
         && !window_minimized
         && !reduced_motion
  ```

  `any_catalog_loading` debe ser `false` cuando todos los catálogos estén en `Ready` o `Failed`. Al desaparecer el último skeleton, los wakeups de animación deben caer **exactamente a 0**. Un timer que sigue corriendo “por las dudas” reintroduce P0-B con otra forma.

### 7.2 Filas y carruseles

- Entrada con `opacity` + `translate` compositivo.
- No animar `y` dentro de `ListView`.
- Limitar la animación a las filas visibles.
- Evitar reanimar en cada `set_row_data`; usar un `loaded` por id, no por índice.

### 7.3 Board

- Skeleton de sección mientras `Loading`.
- Estado `Failed` con mensaje y botón “Reintentar” (no reintento automático infinito).
- Transición `Loading → Ready` con crossfade de 150 ms, sin cambiar la altura de la sección.

### 7.4 Criterios de aceptación

- 200 skeletons: ≤ 1 timer activo y CPU de carga < 25 % en un equipo de referencia.
- Con la carga terminada (`Ready`/`Failed`): **0 wakeups de animación** y 0 timers de skeleton corriendo.
- Scroll de 60 s: dropped frames < 1 % **relativo al refresh real del monitor**, frame p95 ≤ budget (`1000/Hz`).
- Sin cambios de layout durante la animación (medir `y` de la fila).
- `reduced-motion` desactiva todo.
- Minimizar la ventana detiene el driver; restaurarla lo reanuda solo si sigue habiendo carga.

**Estimación Fase 3:** 1–1.5 días.

---

## 8. Fase 4 — Tray monocromático real (Linux + Windows)

**Objetivo:** icono visible y legible en temas claros y oscuros, en ambas plataformas.

### 8.1 Linux (SNI/ksni)

**Hechos:**

- `vendor/i-slint-core/items/system_tray/ksni.rs:44-46`: `icon_name = "stremio-oxide"`.
- `ksni.rs:65-67`: publica un solo `icon_pixmap` de 24×24.
- No hay icono `stremio-oxide` instalado en `hicolor`.
- El `.desktop` de Flatpak usa `Icon=com.stremio.StremioNative.Devel`.

**Problema:** muchos hosts SNI priorizan `icon_name` sobre `icon_pixmap`; si el nombre no existe en el theme, muestran blank/placeholder. Un solo tamaño 24×24 tampoco cubre escalados.

**Solución:**

1. Empaquetar el icono en `hicolor` en varios tamaños: 16, 22, 24, 32, 48, 64, 128, 256.
2. Instalar como `stremio-oxide` (o alinear `icon_name` con el nombre real del `.desktop`).
3. Publicar múltiples `icon_pixmap` (no uno solo) para fallback.
4. Verificar en GNOME (AppIndicator), KDE, XFCE y COSMIC.
5. Para Flatpak, declarar el icono en el manifest y en `finish-args`/`export`.

### 8.2 Windows

**Hechos:**

- `app/src/tray/windows.rs:215-237`: carga `TRAYICON` desde `app/assets/tray.ico`.
- `scripts/gen-tray-ico.ps1`: pinta **blanco puro**, sin contorno.
- Cambiar `app/ui/assets/images/tray-icon.svg` **no afecta Windows**.

**Problema:** en taskbar clara el glifo blanco desaparece.

**Solución:**

1. Regenerar `app/assets/tray.ico` con glifo blanco + contorno oscuro (o dos variantes según tema).
2. Incluir tamaños 16/20/24/32/48/64.
3. Detectar tema (`AppsUseLightTheme`) y elegir variante.
4. Añadir fallback si el `.ico` no carga: no dejar el tray sin icono.
5. Test visual en Windows 10/11, tema claro/oscuro, escala 100/125/150/200 %.

### 8.3 Diseño del glifo

- Monocromo, alto contraste, legible a 16 px.
- Área segura: no tocar el borde.
- Sin `class="color-scheme"` si el rasterizador no lo interpreta (verificar en Slint).
- Validar el SVG con `resvg`/`librsvg` y comparar con el `.ico`.

### 8.4 Criterios de aceptación

- Linux: icono visible en GNOME/KDE en tema claro y oscuro, sin placeholder.
- Windows: icono visible en taskbar clara y oscura, a 100–200 % de escala.
- Cambiar de tema en caliente actualiza el icono (o al menos al reiniciar).
- Sin warnings de icono faltante en logs.

### 8.5 Toggle con clic izquierdo (Linux + Windows)

**Objetivo:** un clic izquierdo en el icono de bandeja muestra u oculta la ventana principal, sin abrir el menú.

Comportamiento:

1. Si la ventana está **visible y enfocada**: guardar geometría y ocultarla a la bandeja.
2. Si está **oculta, minimizada o sin foco**: mostrarla, restaurarla, desminimizarla y traerla al frente.
3. El menú contextual (clic derecho) sigue abriendo el panel/menú completo.
4. El ítem “Open Stremio Oxide” del menú siempre muestra la ventana; no alterna.
5. Si PiP está activo, no ocultar el video: restaurar la ventana completa, igual que el botón de cerrar.
6. En Windows, deduplicar `WM_LBUTTONUP` y `NIN_SELECT` para que un clic físico no alterne dos veces.
7. En Wayland/KDE, si el compositor ignora el primer `focus_window()`, reintentar una vez después de mapear la superficie. No destruir/recrear la superficie salvo que las pruebas demuestren que es imprescindible.

Criterios de aceptación:

- Clic izquierdo sobre ventana enfocada → se oculta y desaparece de la taskbar.
- Clic izquierdo sobre ventana oculta → aparece, se restaura y recibe foco.
- Clic izquierdo sobre ventana visible pero tapada → pasa al frente.
- Clic derecho → menú/panel, sin alternar la ventana.
- Un solo clic produce una sola transición (sin doble toggle).
- Probar en KDE Plasma Wayland y, si hay acceso, Windows 10/11.

**Estado (2026-11-03):** implementado.

- `app/ui/app.slint`: nuevo callback `toggle-window`; el clic izquierdo del tray lo invoca.
- `app/src/tray.rs`: `toggle_window()` decide ocultar solo si la ventana está visible **y** enfocada; en caso contrario llama a `show_window()`. PiP activo restaura la ventana completa. Retry de foco a los 120 ms en Wayland.
- `app/src/tray/windows.rs`: `WM_LBUTTONUP`, `NIN_SELECT` y `NIN_KEYSELECT` pasan por `should_toggle_now()` (debounce de 300 ms); el clic derecho conserva el panel.
- `app/src/tray/standard.rs`: enlaza `on_toggle_window` para Linux/KSNI.
- “Open Stremio Oxide” sigue usando `queue_show_window()` (siempre muestra, nunca alterna).

Validado end-to-end en KDE Plasma Wayland (KWin 6.8.2): invocando `org.kde.StatusNotifierItem.Activate` por D-Bus, la secuencia fue `shown` → `hidden` → `shown`, y el ítem de menú “Open Stremio Oxide” mostró la ventana estando oculta. Un clic sobre ventana visible sin foco la trae al frente en lugar de ocultarla. **Pendiente:** prueba manual en Windows 10/11 (solo verificado por compilación).

**Estimación Fase 4:** 1–1.5 días + pruebas de plataforma.

---

## 9. Fase 5 — Fiabilidad de red: DoH real

**Objetivo:** cumplir `RoadmapConsistency.md` Módulo A, no un parche de URL.

### 9.1 Problema actual

- `core-env/src/lib.rs:333-350`: ante un fallo, resuelve por DoH y **reescribe la URL** a `https://IP/...`.
- Eso rompe SNI/Host, certificados y redirecciones; no es un resolver.
- `DNS_CACHE` se inserta (`:338`) pero nunca se lee.
- `danger_accept_invalid_certs(true)` en el cliente DoH (`:34`).

### 9.2 Solución

1. Implementar `reqwest::dns::Resolve`:
   - `resolve(domain)` consulta cache.
   - Si no hay, resuelve por sistema.
   - Si la IP es bogo/privada/NXDOMAIN, consulta DoH (Cloudflare → Google → Quad9).
   - Guarda en cache con TTL y negativo cache.
2. Mantener el `Host`/SNI original; nunca reescribir la URL.
3. Validación TLS normal (quitar `danger_accept_invalid_certs`; usar IP directa solo con `resolve` o un cliente dedicado y justificado).
4. Timeout y circuit breaker por proveedor.
5. Telemetría: `doh.bypass.count`, `doh.provider`, `doh.latency`, `doh.cache.hit`.
6. Tests: dominio con IP privada simulada, NXDOMAIN, timeout, cache hit, fallback de proveedor.

### 9.3 Criterios de aceptación

- Un host bloqueado por DNS del ISP carga por DoH sin cambiar la URL.
- Cache hit evita la segunda consulta.
- Sin `danger_accept_invalid_certs` en el path normal.
- No hay regresiones en addons HTTPS.

**Estimación Fase 5:** 2–4 días.

---

## 10. Fase 6 — Fiabilidad de datos: DB, locks y watchdogs

### 10.1 Turso / SQLite

**Hecho verificado:**

- `app/src/db.rs:42-54` ya ejecuta `PRAGMA journal_mode = WAL` (por query, porque devuelve fila) y solo emite un `warn` si no queda en `wal`.
- `app/src/db.rs:58-62`: `synchronous = NORMAL`, `foreign_keys = ON`, `temp_store = MEMORY`, `cache_size = -4000`, `mmap_size = 30000000`.
- `core-env/src/lib.rs:608-610`: `install_database` reutiliza la misma `turso::Database`; `get_db_conn` (`:636-668`) abre conexiones nuevas con `synchronous = NORMAL`, `busy_timeout = 1000`, etc.
- `core-env/src/lib.rs:404-405`: un único `AsyncMutex` serializa **todo** el acceso a la conexión compartida.
- Turso `0.7.2` (`Cargo.toml:71`). `Connection::clone()` es un `Arc` clone y comparte estado de transacción; para un pool hay que usar `db.connect()`.

**Solución (en este orden):**

1. **WAL hard-fail:** si `journal_mode != wal`, abortar `init_db` con error accionable en vez de continuar con un `warn`. Un fallback silencioso a rollback journal es peor que fallar.
2. **Verificación idempotente en `core-env`:** antes de crear conexiones nuevas, ejecutar/verificar `PRAGMA journal_mode = WAL` y `PRAGMA synchronous = NORMAL`. No asumir que la base siempre pasó por `init_db`.
3. **Spike/benchmark previo al pool:** medir con Turso 0.7.2 si 2–4 conexiones de lectura creadas con `db.connect()` + `PRAGMA query_only = ON` realmente leen en paralelo bajo WAL. No avanzar sin números.
4. **Si el spike da positivo:** una conexión de escritura detrás de un canal MPSC con batching + 2–4 conexiones de solo lectura; eliminar el `AsyncMutex` global. Si da negativo, mantener el mutex y optimizar batching/mantenimiento.
5. Batch de escrituras ya existe (`PENDING_STORAGE_WRITES`); asegurar que el flush no bloquee lecturas.
6. Añadir test de contención con 100 lecturas + 1 mantenimiento y medir `db.wait.duration` p95/p99.

**Criterio de aceptación:** 100 lecturas concurrentes no superan 50 ms p95 por lectura; el mantenimiento no produce picos > 100 ms; WAL verificado en runtime.

### 10.2 Watchdogs

- Mantener el watchdog de salida de 6 s (`main.rs:337,381`), pero verificar que no compita con el cierre limpio.
- Revisar el retry/watchdog del servidor (`main.rs:629-716`): backoff 400 ms → 4 s; asegurar que no spawnee múltiples servidores.
- **No reintroducir** el watchdog destructivo de arranque eliminado en `519a3e9`.
- Añadir un test que simule servidor que no responde y verifique un solo proceso.

### 10.3 Toasts (Módulo D)

- Crear `app/src/toast_manager.rs` y `app/ui/components/toast.slint`.
- API: `show(kind, message, dedupe_key, ttl)`.
- Deduplicación por `dedupe_key` y ventana de tiempo.
- Cola con máximo N visibles; no bloquear UI.
- Integrar: “Bloqueo DNS evitado”, “DPI detectado”, “Servidor reiniciado”, “Catálogo falló”.
- `ToastItem` ya existe en `types.slint:26`; reutilizarlo.

### 10.4 Criterios de aceptación

- 100 lecturas concurrentes no superan 50 ms p95 por lectura.
- WAL y `synchronous = NORMAL` verificados en runtime; hard-fail si no.
- El pool de lectores (si se implementa) está respaldado por un benchmark, no por una suposición.
- Un servidor caído produce exactamente un reinicio por backoff, no un loop.
- Los toasts se deduplican y desaparecen solos; no roban foco.

**Estimación Fase 6:** 3.5–4.5 días (incluye el spike de concurrencia Turso).

---

## 11. Fase 7 — Verificación, métricas y no-regresión

### 11.1 Presupuesto según refresh rate (no fijar 60 Hz)

El objetivo depende del monitor y del hardware del usuario. Sea `budget = 1000 / refresh_hz`:

| Hz | Budget por frame | UI thread p95 | UI thread p99 | Frame p95 scroll |
|---|---|---|---|---|
| 60 | 16.67 ms | < 4 ms | < 8 ms | ≤ 16.67 ms |
| 120 | 8.33 ms | < 4 ms | < 8 ms | ≤ 8.33 ms |
| 144 | 6.94 ms | < 4 ms | < 8 ms | ≤ 6.94 ms |
| 165 | 6.06 ms | < 3.5 ms | < 7 ms | ≤ 6.06 ms |
| 240 | 4.17 ms | < 3 ms | < 6 ms | ≤ 4.17 ms |

**Regla:** ningún callback de Rust/Slint en el hilo de UI debe superar el 50 % del budget del monitor. Si el trabajo de UI queda por debajo de 4 ms p95, el renderer nativo (FemtoVG/Skia) puede sostener el VSync sin ayuda. El refresh real se obtiene con `winit::monitor::MonitorHandle::refresh_rate_millihertz()`; **no asumir 60 Hz**.

Si más adelante se agrega un límite configurable de FPS (para batería o preferencia del usuario), el budget efectivo es `1000 / min(refresh_hz, fps_cap)`. La no-regresión se evalúa contra ese budget, no contra un número fijo.

El mínimo garantizado se valida en hardware de gama baja a 60 Hz. Los tiers 120/144/165/240 Hz son presupuesto de no-regresión: si el usuario tiene ese monitor, la app no debe ser la que pierde frames.

**Métricas:**

| Métrica | Objetivo | Cómo se mide |
|---|---|---|
| UI callback p95 / p99 | < 4 ms / < 8 ms (60–144 Hz) | spans en event loop y callbacks |
| UI callback máximo | < 50 % del budget del monitor | idem |
| Dropped frames | < 1 % relativo al refresh real | present timestamps, MangoHud, PresentMon |
| Frame p95 idle | ≤ budget | idem |
| Frame p95 scroll | ≤ budget | idem |
| CPU idle | < 2 % | `top`/`perf` 60 s |
| CPU carga | < 25 % | idem |
| Timers activos con 200 skeletons | ≤ 1 | instrumentación |
| Wakeups de animación tras `Ready`/`Failed` | 0 | contador |
| `LoadRange`/catálogo fallido en 60 s | ≤ 5, con backoff | contador |
| `library.sync` / `discover.sync` p95 | < 4 ms | span |
| `refresh_cached_media_images` p95 | < 2 ms | span |
| Cambio de pestaña p95 | ≤ budget | traza |
| Alineación X | ≤ 1 px | captura/medición |
| Drawer close | 200 ms ± 20 ms, sin snap | grabación |
| Cambios de `grid-width` por transición | ≤ 1 | contador de layouts |
| Tray visible | 100 % casos | matriz manual |

### 11.2 Matriz de pruebas

**Plataformas:**
- Linux: GNOME (Wayland/X11), KDE, XFCE.
- Windows: 10, 11.
- Escalas: 100 %, 125 %, 150 %, 200 %.
- Temas: claro, oscuro, alto contraste.
- `prefers-reduced-motion`: on/off.
- Refresh: 60 Hz, 120 Hz, 144 Hz, 240 Hz (o el máximo del monitor disponible).

**Estados:**
- Sin auth, auth OK, auth error.
- Sin addons, 1 addon, 10 addons, addon que falla.
- Catálogo vacío, 1 página, 20 páginas.
- DB vacía, DB grande, DB con mantenimiento.
- Red OK, DNS bloqueado, timeout, offline.

**Interacciones:**
- Abrir/cerrar drawer 20 veces seguidas.
- Cambiar pestañas rápido 20 veces.
- Scroll continuo 60 s.
- Cambiar preset de layout en caliente.
- Minimizar/restaurar.
- Cerrar con tray activo.
- Redimensionar la ventana cruzando el umbral overlay/push.

### 11.3 Pruebas automáticas mínimas

- Unit: máquina de estados de catálogo (sin retry infinito).
- Unit: `bounded_catalog_load_range` no incluye `Err`.
- Unit: cache DoH (hit/miss/negativo).
- Unit: dedupe de toasts.
- Integración: 100 lecturas + mantenimiento DB.
- Snapshot/visual: geometría en 3 resoluciones × 3 presets.
- Smoke: arranque, carga de Board, apertura/cierre de preview, tray.

---

## 12. Riesgos y mitigaciones

| Riesgo | Probabilidad | Impacto | Mitigación |
|---|---|---|---|
| El WIP contiene piezas útiles y se pierden al limpiar | Media | Medio | Tag `wip-ui-attempt-1` + commits separados |
| Cambiar `Err` a terminal oculta fallos transitorios | Media | Alto | Retry manual + backoff acotado + telemetría |
| Interpolar `grid-width` reintroduce reflow | Media | Alto | Regla dura: ≤ 1 cambio por transición; medir layouts |
| El modo overlay vs push agrega complejidad | Media | Medio | Flag `ui.preview-push` + A/B por ancho |
| `mi_collect` en background no libera el heap de UI | Alta | Medio | Ejecutarlo solo en UI oculta/minimizada; configurar purge |
| Objetivos de FPS mal calibrados | Media | Medio | Budget `1000/Hz`; medir refresh real |
| El tray Linux depende del host SNI | Alta | Medio | `icon_name` + múltiples `icon_pixmap` + hicolor |
| Windows `.ico` no se regenera en CI Linux | Media | Medio | Script reproducible + artefacto versionado + test |
| DoH con IP directa rompe TLS | Media | Alto | `Resolve` real, no reescritura de URL |
| WAL no soporta lecturas concurrentes en Turso 0.7.2 | Media | Alto | Spike/benchmark antes de eliminar el mutex |
| Pool de DB introduce inconsistencias | Baja | Alto | Conexiones separadas + transacciones + tests |
| Regresión de arranque por watchdogs | Media | Alto | Un solo supervisor + test de proceso único |

### Rollback

- Cada fase en commits atómicos con `git revert` posible.
- Feature flags para: `ui.new-layout`, `ui.preview-push`, `ui.preview-overlay`, `net.doh-resolver`, `db.read-pool`, `ui.toasts`.
- Build A/B: `v0.99.82+fix0`, comparar métricas.
- Si una fase no mejora las métricas, se revierte sin tocar las demás.

---

## 13. Orden de ejecución y estimación

| Fase | Contenido | Depende de | Estimación |
|---|---|---|---|
| 0 | Stop the bleeding: bucle, timers, locks, instrumentación | — | 3–4 días |
| 1 | Alineación geométrica | 0 | 1.5–2 días |
| 2 | Preview drawer (overlay/push + anti-reflow) | 1 | 2–2.5 días |
| 3 | Animaciones de carga/transiciones | 0, 2 | 1–1.5 días |
| 4 | Tray Linux/Windows | 0 | 1–1.5 días + pruebas |
| 5 | DoH real | 0 | 2–4 días |
| 6 | DB/locks/watchdogs/toasts (+ spike Turso) | 0 | 3.5–4.5 días |
| 7 | Verificación completa | 1–6 | 2–3 días |

**Total estimado:** 16–23 días de trabajo efectivo, excluyendo pruebas de plataforma y revisiones.

**Hito mínimo viable (MVP de rescate):** Fases 0 + 1 + 2 + 4. Con eso la app deja de stutter, se alinea y el tray se ve. Fases 3, 5, 6 y 7 completan la calidad.

**Nota sobre FPS/Hz:** el MVP se valida primero a 60 Hz en hardware modesto. La suite de 120/144/240 Hz se corre como no-regresión; no se exige hardware de gama alta para aprobar.

---

## 14. Checklist de aceptación final (Definition of Done)

### Stutter
- [ ] Un catálogo que falla no genera más de 5 `LoadRange` en 60 s.
- [ ] No hay `Timer` de intervalo < 100 ms por instancia de UI.
- [ ] No se animan `y`/`width`/`height` en listas virtualizadas.
- [ ] `grid-width` no se interpola por frame (≤ 1 cambio por transición).
- [ ] `sync` de Library/Discover suelta el lock antes del mapeo.
- [ ] `refresh_cached_media_images` no recorre toda la pestaña si no hay URLs nuevas.
- [ ] `mi_collect` no corre en navegación; solo en minimize/hidden/tray, en el hilo de UI.
- [ ] UI callback p95 < 4 ms y p99 < 8 ms.
- [ ] Frame p95 ≤ `1000/refresh_hz`; dropped frames < 1 %.
- [ ] Wakeups de animación = 0 cuando no hay catálogos cargando.
- [ ] CPU idle < 2 %.

### UI
- [ ] Buscador, filtros y primer póster alineados ≤ 1 px en 3 resoluciones × 3 presets.
- [ ] Fila de filtros sin overflow a 1366px y con drawer abierto.
- [ ] Drawer: una animación, sin gutter, sin snap, X y toggle sincronizados.
- [ ] Drawer overlay o push atómico: sin reflow por frame.
- [ ] i18n de “Mostrar/Ocultar vista previa”.
- [ ] Skeletons y transiciones sin reflow; `reduced-motion` respetado.

### Tray
- [ ] Linux: icono visible en GNOME/KDE, claro/oscuro.
- [ ] Windows: icono visible en taskbar clara/oscura, 100–200 %.
- [ ] Sin warnings de icono faltante.

### Fiabilidad
- [ ] DoH vía `Resolve`, cache leída, TLS válido.
- [ ] WAL verificado en runtime con hard-fail; `synchronous = NORMAL`.
- [ ] Pool de lectores solo si el benchmark de Turso 0.7.2 lo respalda.
- [ ] DB con lecturas concurrentes sin bloqueo > 50 ms p95.
- [ ] Watchdogs no duplican procesos.
- [ ] Toasts deduplicados y no bloqueantes.

---

## 15. Anexo: inventario de archivos por fase

### Fase 0
- `app/src/models/board.rs` (máquina de estados, retry)
- `app/src/models/mod.rs` (`visible_catalog_load_range`, `refresh_cached_media_images`)
- `app/src/models/library.rs`, `app/src/models/discover.rs` (locks)
- `app/src/event_loop.rs`, `app/src/main.rs` (`mi_collect`)
- `app/ui/components/feedback.slint`, `app/ui/components/media_carousel.slint` (timers)
- `app/src/startup_metrics.rs` (instrumentación)

### Fase 1
- `app/ui/app.slint` (navbar, `content-left-inset`)
- `app/ui/theme.slint` (`vertical-nav-bar-size`, nueva variable)
- `app/ui/pages/library.slint`, `app/ui/pages/discover.slint` (filtros, grid)
- `app/ui/components/media_card.slint` (offset)
- `app/ui/components/sidebar.slint` (visibilidad/preset)

### Fase 2
- `app/ui/pages/library.slint`, `app/ui/pages/discover.slint` (estado preview)
- `app/ui/components/preview_drawer.slint` (`progress`, X, width)
- `app/ui/components/button.slint` (toggle superior derecho)
- i18n: archivos de traducción

### Fase 3
- `app/ui/components/feedback.slint`
- `app/ui/components/media_carousel.slint`
- `app/ui/components/loading_placeholders.slint`
- `app/ui/pages/board.slint`

### Fase 4
- `app/ui/assets/images/tray-icon.svg`
- `app/assets/tray.ico`
- `scripts/gen-tray-ico.ps1`
- `app/src/tray/windows.rs`, `app/src/tray/standard.rs`
- `vendor/i-slint-core/items/system_tray/ksni.rs`
- `flatpak/com.stremio.StremioNative.Devel.desktop`
- instalación `hicolor` (empaquetado/CI)

### Fase 5
- `core-env/src/dns.rs`, `core-env/src/lib.rs`
- tests de red

### Fase 6
- `core-env/src/lib.rs` (DB)
- `app/src/main.rs` (watchdogs)
- `app/src/toast_manager.rs` (nuevo)
- `app/ui/components/toast.slint` (nuevo)
- `app/ui/components/types.slint` (`ToastItem`)

### Fase 7
- `Roadmap_Maestro_UI_Resiliencia.md` (este documento)
- scripts de medición
- CI de regresión visual/métricas

---

## 16. Próximo paso

**No implementar todavía.** El orden recomendado es:

1. Aprobar este roadmap (incluida la revisión de la auditoría en §3.5).
2. Ejecutar **Fase 0** en una rama `fix/p0-stutter`, con commits atómicos y métricas antes/después.
3. Recién con P0 verificado, avanzar a Fase 1 (geometría) y Fase 2 (drawer).
4. Tray (Fase 4) en paralelo si hay acceso a Windows para validar.

**Decisiones que deben cerrarse antes de codificar:**

- [ ] Drawer: ¿overlay siempre, push siempre, o híbrido por ancho? (recomendado: híbrido con flag).
- [ ] `mi_collect`: ¿se elimina por completo o queda solo en minimize/tray? (recomendado: solo oculto).
- [ ] `mi_option_purge_delay`: valor a fijar tras medir RSS (no hardcodear sin datos).
- [ ] Pool Turso: solo si el spike de concurrencia con WAL da positivo.
- [ ] Refresh objetivo: validar 60 Hz en hardware mínimo y usar el Hz real del monitor para no-regresión.

> Mientras la Fase 0 no esté cerrada, cualquier cambio visual nuevo va a volver a “fracasar” porque el stutter y los retries seguirán contaminando la percepción.

---

## 17. Estado de implementación (2026-11-03)

Rama de trabajo: `fix/rescue` (el intento anterior quedó preservado en `wip/ui-attempt-1`).

### Implementado

- **P0-A — bucle de catálogos:** `Err` dejó de considerarse “no cargado”; el kick inicial tiene backoff de 2 s y solo reintenta `None`/`Loading`. La proyección ya no incluye el texto del error en el fingerprint.
- **P0-B/P0-C — timers y reflow:** eliminados los `Timer` de 16 ms por skeleton y por fila; el shimmer infinito se reemplazó por placeholders estáticos. Se quitaron las animaciones de `y` en listas y el timer de 35 ms por tarjeta. El drawer conserva una sola animación de opacidad de 180–200 ms.
- **P1-A — locks:** Library y Discover clonan sus tablas de lookup bajo el lock y lo sueltan antes del fingerprinting/mapeo O(n).
- **P1-B — refresco de imágenes:** `refresh_cached_media_images` retorna temprano si no hay URLs o no hay imágenes decodificadas.
- **P1-C — mimalloc:** eliminado `trim_process_memory(false)` de navegación/arranque y el timer de 60 s que no recolectaba el heap de UI. Solo quedan los trims agresivos en minimize/descarga de reproductor.
- **Fase 1 — alineación:** `page-left-inset` unifica buscador, filtros y primera card en Board/Library/Discover; el navbar dejó de centrar el buscador; el sort de Library pasó a `SelectField`; la primera card arranca en el mismo eje que los filtros.
- **Fase 2 — preview:** botón en el extremo derecho de la fila superior, i18n `PREVIEW_SHOW`/`PREVIEW_HIDE`, X que desactiva `preview-enabled` y persiste, cierre con una sola animación, sin gutter, y columnas que se recalculan al abrir/cerrar.
- **Fase 3 — animaciones:** skeletons estáticos, sin animación de `y`, sin reanimación al reciclar.
- **Fase 4 — tray:** SVG con contorno oscuro; en Linux `IconName` vacío para que el `IconPixmap` 24×24 sea autoritativo (verificado por D-Bus); `tray.ico` regenerado con 8 tamaños y contorno oscuro para Windows.
- **Fase 5 — DoH:** `reqwest::dns::Resolve` real (`DohResolver`) con cache, fallback Cloudflare/Google/Quad9, NXDOMAIN y Bogo-IP; la URL y el SNI originales se preservan.
- **Fase 6 — DB y toasts:** WAL ahora es hard-fail en `init_db`; se agregó `toast_manager` + `ToastStack` y se emite un toast deduplicado ante error de catálogo. El pool de lectores de Turso queda pendiente del benchmark indicado en §10.1.
- **Build:** `cargo build --release -p stremio-native` OK. `cargo test -p stremio-native --bins`: 159/159. `cargo test -p core-env`: 13/13.

### Pendiente / requiere hardware o benchmark

- Pool de conexiones Turso: no se implementó a ciegas; falta el spike de concurrencia con WAL.
- Instrumentación de frame time y refresh rate: queda como trabajo de medición (no bloquea el build).
- QA visual en Windows 10/11 (tray claro/oscuro, escalas 125–200 %), GNOME/KDE/XFCE y monitores 120/144/240 Hz.
- `ToastStack` cubre el caso de error de catálogo; faltan los toasts de DoH/DPI y watchdog del servidor.
