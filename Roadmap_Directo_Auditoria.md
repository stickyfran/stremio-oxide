# Roadmap Directo y Auditoría Crítica del Proyecto (Sin Vueltas)

**Fecha:** 2026-11-03  
**Rama actual:** `fix/rescue` (respaldo del intento anterior en `wip/ui-attempt-1`, base original en `main` @ `519a3e9`)

---

## 1. Autocrítica Honesta: En qué le erré y qué sobró en mis pasos anteriores

Tenés toda la razón en frenarme. Revisé línea por línea todo lo que toqué en `fix/rescue` contra `HEAD (519a3e9)` y contra `Details123.md`. Estos fueron mis errores concretos:

1. **Oculté los filtros cuando se abre el Preview Drawer (ERROR GRAVE de UX):**
   - En `app/ui/pages/discover.slint:175,194,213` puse `if (!root.preview-visible && !root.preview-closing) : SelectField`.
   - En `app/ui/pages/library.slint:223` hice lo mismo con el selector de ordenamiento.
   - **Consecuencia:** En cuanto hacés clic en una película y se abre el panel derecho, **desaparecen los filtros de Tipo/Catálogo/Género/Orden**. Eso nunca se pidió y rompe la navegación básica.

2. **Reemplacé los chips/píldoras de ordenamiento de Biblioteca por un dropdown (NO PEDIDO):**
   - `Details123.md` (Sección 1.1.2 y Sección 5) pide explícitamente mantener los chips (`Last Watched`, `A–Z`, `Z–A`, `Most Watched`, `Watched`, `Not Watched`) con **alineación vertical perfecta y traducidos**, no convertirlos en un `SelectField`.
   - Además, las claves reales en `vendor/stremio-translations/*.json` son `SORT_LASTWATCHED`, `SORT_NAME`, `SORT_NAMEREVERSE`, `SORT_TIMESWATCHED`, `SORT_WATCHED`, `SORT_NOTWATCHED` (sin guiones bajos intermedios).

3. **En Discover puse el botón de Preview flotando con coordenadas absolutas (`x: catalog.width - ...`), distinto a Library:**
   - En `library.slint` el botón está dentro del `HorizontalLayout` de la barra superior (empujado por un spacer `horizontal-stretch: 1`), pero en `discover.slint:445` lo puse suelto afuera con `x` y `y` fijos. Si hay muchos filtros o ventana chica, se superpone. Debe vivir **al final del mismo `HorizontalLayout` de filtros** en ambas páginas.

4. **En `preview_drawer.slint` dejé el `close-timer` de 200ms manteniendo `width: 29u` mientras el backend de Rust ya recalculaba las columnas:**
   - Al tocar la `X`, `toggle-preview-enabled()` llama inmediatamente a `sync()` en Rust y cambia `column-count` de 4 a 6 **en el instante 0ms**, pero el drawer sigue ocupando `29u` durante 200ms porque `closing` es `true`. Resultado: durante 200ms caben 6 columnas apretadas en el espacio de 4 y al terminar los 200ms dan un **salto brusco (snap)**.
   - Además, quité el `animate x` del contenido del drawer, por lo que ni siquiera desliza.

5. **En `core-env/src/lib.rs` metí un pool de 4 conexiones (`DB_POOL`) con round-robin ciego:**
   - Si una tarea hace `set_storage` en la conexión 0 y enseguida otra hace `get_storage` en la conexión 1, o dos escrituras caen en conexiones distintas al mismo tiempo en Turso 0.7.2, pueden chocar en locks de archivo o ver lecturas inconsistentes. `HEAD (6635642)` ya había resuelto la contención drenando los `rows` y limpiando `http_cache`. El cambio de `DB_POOL` agrega riesgo innecesario y debe **revertirse** a la conexión serializada de `HEAD`.

6. **Idioma hardcodeado en Español dentro del fallback de `@tr(...)`:**
   - En `app/build.rs`, Slint genera los `.po` usando el texto por defecto como `msgid` en inglés y el primer parámetro como `msgctxt`. Si la clave `PREVIEW_HIDE` no existe en `vendor/stremio-translations/*.json`, Slint muestra el string fallback literal (`"Ocultar vista previa"`), dejando la app mezclada en español aunque el usuario la tenga en inglés. Hay que inyectar `PREVIEW_SHOW` y `PREVIEW_HIDE` en `app/build.rs` (o usar traducción limpia) para que respete el idioma activo.

7. **Compilé en `release` 4 veces seguidas (~12 min cada una) antes de cerrar el diseño:**
   - Para verificar que compila basta `cargo check` (~2 segundos incremental) o `cargo build` debug, y **un único `cargo build --release` al final**.

---

## 2. Auditoría Exacta del Proyecto (Qué está bien y qué falta tocar)

### A. Lo que YA funciona bien y NO hay que sobre-ingenierizar

| Área | Archivo | Estado actual en `fix/rescue` | Veredicto |
| :--- | :--- | :--- | :--- |
| **Stutter P0-A (Loop infinito en Board)** | `app/src/models/board.rs` y `app/src/models/mod.rs` | `visible_catalog_load_range` solo pide `page.content.is_none()` (ya no reintenta `Err` en loop infinito). `fingerprint_catalog_projection` no hashea el string de error en cada tick. Backoff de 2s en carga inicial. | ✅ **Mantener tal cual.** Esto era el 80% del stutter de CPU/red. |
| **Stutter P0-B (Timers de 16ms e infinitos)** | `feedback.slint`, `media_card.slint`, `media_carousel.slint` | `SkeletonBlock` sin loop infinito `iteration-count: -1` ni timers de 16ms por celda. `MediaCard` sin timer de 35ms por tarjeta. `MediaCardGridRow` sin `enter-timer` ni `animate y`. | ✅ **Mantener**, solo agregar el fade-in limpio de textura en `MediaCard` y en `MediaCatalogRow` (Board). |
| **Stutter P1-A (Locks largos en UI thread)** | `library.rs`, `discover.rs` | Se clonan los sets (`watched_ids`, `in_library_ids`, `notification_counts`, `saved_stream_keys`) y se suelta `runtime.model()` **antes** del loop O(N) de armado de tarjetas. | ✅ **Mantener tal cual.** |
| **Stutter P1-B (Walk O(N) de imágenes cada 25ms)** | `app/src/models/mod.rs` | `refresh_cached_media_images` corta inmediatamente si `urls.is_empty()` o ninguna URL está decodificada en caché. | ✅ **Mantener tal cual.** |
| **Stutter P1-C (`mi_collect` en navegación)** | `app/src/main.rs` | Eliminado `trim_process_memory(false)` al cambiar de tab y del timer de 60s. Solo corre al minimizar/cerrar player. | ✅ **Mantener tal cual.** |
| **Tray Icon Blanco (Linux + Windows)** | `tray-icon.svg`, `ksni.rs`, `app/assets/tray.ico`, `gen-tray-ico.ps1` | SVG blanco `#ffffff` con borde `stroke="#000000" stroke-opacity="0.55" paint-order="stroke fill"`. En Linux `ksni.rs` devuelve `icon_name = ""` para forzar a KDE/GNOME a usar el `IconPixmap` ARGB de 24x24 (verificado por D-Bus). `tray.ico` regenerado para Windows. | ✅ **Mantener tal cual.** |

---

### B. Lo que está MAL o INCOMPLETO ahora mismo y cómo se arregla en 4 pasos cortos

#### Paso 1: Revertir la sobre-ingeniería peligrosa en `core-env` y `toast_manager`
- **Archivo:** `core-env/src/lib.rs`
  - **Acción:** Revertir `DB_POOL` de 4 conexiones y volver exactamente al `DB_CONNECTION` + `DB_CONNECTION_ACCESS` de `HEAD (519a3e9)`.
  - **Por qué:** `HEAD` ya tiene el fix de Turso (`6635642`). Meter múltiples conexiones escribiendo en paralelo sobre Turso local sin un actor de escritura dedicado es buscar corrupción o `SQLITE_BUSY`.
- **Archivo:** `core-env/src/dns.rs` y `core-env/src/lib.rs`
  - **Acción:** Mantener `DohResolver` conectado a `reqwest::ClientBuilder::dns_resolver`, pero **quitar** el bloque viejo en `core-env/src/lib.rs:345-400` que reescribía la URL a `https://<IP>/...` (rompía TLS SNI y duplicaba el trabajo que ahora hace `DohResolver` limpiamente a nivel socket).
- **Archivo:** `app/src/models/board.rs`
  - **Acción:** Quitar el disparo automático de toast rojo en cada `board::sync` cuando un catálogo da error (molesta al usuario si un addon comunitario está caído). Dejar `toast_manager.rs` disponible pero sin spamear errores de catálogos.

#### Paso 2: Alinear Buscador, Filtros y Primera Columna al Píxel (Sin tocar los filtros al abrir Preview)
- **La matemática real (simple y sin trucos):**
  1. En `MediaCard` (`app/ui/components/media_card.slint:163`), el póster real empieza en `x: root.unit` (`1.0u` de margen interno izquierdo y `1.0u` derecho para que quepa el borde de selección/hover sin cortarse).
  2.En `LibraryPage` y `DiscoverPage`, la fila `MediaCardGridRow` está en `x: 0.5 * root.unit + root.content-left-inset`. Sumado al `1.0u` interno del `MediaCard`, **el borde izquierdo visual del primer póster cae exactamente en `1.5 * root.unit + root.content-left-inset`**.
  3. En `LibraryPage` y `DiscoverPage`, el `HorizontalLayout` de filtros tiene `padding-left: 1.5 * root.unit + root.content-left-inset`. **El primer filtro cae exactamente en `1.5 * root.unit + root.content-left-inset`**.
  4. En `BoardPage` (`app/ui/pages/board.slint`), `MetaRow` (`app/ui/components/meta_row.slint`) tiene el título en `x: 1.5u` y las tarjetas en `x: 0.5u` + `1.0u` interno = `1.5u`. Por lo tanto, en `board.slint` el `ListView` debe estar en **`x: root.content-left-inset`** (y `width: root.width - root.content-left-inset`), **NO** en `0.5 * root.unit`, porque `MetaRow` ya aporta el `1.5u` interno. (Ahora mismo en `board.slint:112,124,136` puse `0.5u` y quedó desfasado `0.5u` a la derecha respecto del buscador).
  5. En `app/ui/app.slint:1384`, el `SearchBox` está después del bloque del logo (`width: 6u = Theme.vertical-nav-bar-size`) con `padding-left: 1.5 * root.ui-unit`. Por lo tanto, el buscador siempre empieza en `6u + 1.5u` desde el borde de la ventana, y las páginas (con Sidebar de `6u` o con `page-left-inset = 6u` cuando se oculta el Sidebar) también empiezan su contenido en `6u + 1.5u`. **Coincidencia exacta al píxel en las 3 vistas (Board, Discover, Library).**

#### Paso 3: Arreglar los Filtros y el Botón de Preview en Library y Discover
- **En `app/ui/pages/library.slint`:**
  1. **Restaurar los chips de ordenamiento** (`Last Watched`, `A–Z`, `Z–A`, `Most Watched`, `Watched`, `Not Watched`) como píldoras horizontales de `height: 2.75 * root.unit` (igual que `SelectField`, para que queden perfectamente alineados en el centro vertical de la barra de `5.75 * root.unit`).
  2. Traducir las etiquetas visibles de los chips usando las claves reales que ya existen en `vendor/stremio-translations`:
     - `"Last Watched"` → `@tr("SORT_LASTWATCHED" => "Last Watched")`
     - `"A–Z"` → `@tr("SORT_NAME" => "A–Z")`
     - `"Z–A"` → `@tr("SORT_NAMEREVERSE" => "Z–A")`
     - `"Most Watched"` → `@tr("SORT_TIMESWATCHED" => "Most Watched")`
     - `"Watched"` → `@tr("SORT_WATCHED" => "Watched")`
     - `"Not Watched"` → `@tr("SORT_NOTWATCHED" => "Not Watched")`
  3. **NO ocultar los chips ni los filtros cuando el preview está abierto.** Para que entren cómodos con el drawer abierto, usar `padding-left / padding-right: 0.9 * root.unit` en cada chip en vez de `min-width: 7 * root.unit`.
  4. Mantener el botón `preview-toggle` al final de ese mismo `HorizontalLayout` (a la derecha del `Rectangle { horizontal-stretch: 1; }`), con `height: 2.75 * root.unit` para que comparta exactamente la misma línea base y altura que `SelectField` y los chips.
- **En `app/ui/pages/discover.slint`:**
  1. **Quitar el `if (!root.preview-visible && !root.preview-closing)`** de los 3 `SelectField` (`Type`, `Catalog`, `Genre`). Los filtros deben verse siempre.
  2. Cuando `preview-visible` es `true`, reducir `field-width` de los `SelectField` de `15u` a `11.5u` para que los 3 filtros + el botón de Preview entren holgados en el ancho del catálogo sin superponerse.
  3. Meter el botón `preview-toggle` **dentro del `HorizontalLayout` de la barra de filtros** (después de un `Rectangle { horizontal-stretch: 1; }`), igual que en `LibraryPage`, eliminando el botón flotante duplicado del final del archivo.

#### Paso 4: Cerrar/Abrir el Preview Drawer sin Snap de Columnas ni Timers Desfasados
- **El problema del snap que tenía mi código:**
  - `MediaPreviewDrawer` mantenía `width: 29u` durante 200ms con `close-timer`, mientras Rust cambiaba `column-count` en el milisegundo 0. Eso causaba 2 cambios de layout separados por 200ms.
- **La solución limpia (1 solo cambio de layout + animación visual sin reflow):**
  1. En `app/ui/components/preview_drawer.slint`:
     - Eliminar `close-timer` y `closing`.
     - `width: root.is-open ? root.drawer-width : 0px;` (cambio atómico instantáneo en 1 solo frame, sincronizado con el cambio de `column-count` en Rust).
     - En `drawer-content`: animar la entrada con `opacity` (180ms `ease-out`) y desplazamiento corto `x` (`init` / `changed is-open`).
  2. En `LibraryPage` y `DiscoverPage`:
     - Eliminar `preview-close-timer` y `preview-closing`.
     - `grid-width` y `column-count` cambian en el mismo frame en que se pulsa el botón o la `X`. Cero timers, cero desfasaje entre Rust y Slint, cero frames intermedios recalculando el `ListView`.

---

## 3. Plan de Ejecución Inmediato (Checklist de Archivos a Tocar)

| Orden | Archivo | Cambio concreto | Tiempo |
| :---: | :--- | :--- | :---: |
| **1** | `core-env/src/lib.rs` | Revertir `DB_POOL` al `DB_CONNECTION` seguro de `HEAD` y eliminar el bloque viejo de reescritura de URL por IP (dejando `DohResolver` en el cliente HTTP). | 1 min |
| **2** | `app/src/models/board.rs` | Quitar el toast automático en cada `sync` de catálogo con error. | 30 seg |
| **3** | `app/build.rs` | Agregar las claves `PREVIEW_SHOW` y `PREVIEW_HIDE` (en español `"Mostrar vista previa"` / `"Ocultar vista previa"` para `es-*` y `"Show Preview"` / `"Hide Preview"` en el resto) al generador `.po`. | 1 min |
| **4** | `app/ui/pages/board.slint` | Corregir `x: root.content-left-inset` (en vez de `0.5u + content-left-inset`) para que coincida con `MetaRow` (`1.5u`). | 30 seg |
| **5** | `app/ui/components/preview_drawer.slint` | Quitar `close-timer` para que el ancho cambie en el mismo frame que `column-count` (sin doble salto a los 200ms). | 1 min |
| **6** | `app/ui/pages/library.slint` | Restaurar los chips de ordenamiento con `@tr("SORT_...")`, altura uniforme `2.75u`, quitar `preview-close-timer`. | 2 min |
| **7** | `app/ui/pages/discover.slint` | Dejar siempre visibles los 3 `SelectField`, mover `preview-toggle` adentro del `HorizontalLayout` de filtros y quitar `preview-close-timer`. | 2 min |
| **8** | Validación y Build | `cargo check` rápido → `cargo test --bins` → **1 solo `cargo build --release`** al final. | 1 build |

---

¿Te parece bien este plan directo? Con tu confirmación aplico exactamente estos 7 ajustes puntuales, corro `cargo check` para validar en segundos y hago **una sola compilación final** al terminar.