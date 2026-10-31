# Roadmap Técnico y Especificación de Arquitectura — Stremio Oxide

Este documento provee el contexto exhaustivo, diagnóstico de causas raíz a bajo nivel, trazabilidad en el código fuente, la comparación formal de **Cómo estaba (Estado Previo)** versus **Cómo va a quedar (Estado Objetivo)**, los **Objetivos Técnicos y de Negocio**, y las **Restricciones Críticas de Implementación (Guardrails y Edge Cases)** para que cualquier ingeniero de software o agente de IA auditor pueda validar e implementar cada uno de los cambios sin introducir regresiones en producción.

---

## 1. Distribución, Empaquetado y Actualizaciones

### 1.1 Objetivos del Cambio
- **Claridad de entrega al usuario final**: Evitar que el usuario confunda un archivo auxiliar de actualización con una versión portable del cliente.
- **Canal de distribución unificado**: Garantizar que el repositorio oficial para versiones y actualizaciones sea `stickyfran/stremio-oxide`.
- **Integridad del proceso de actualización**: Permitir la actualización desatendida mediante descarga directa del instalador compilado sin depender de wrappers intermedios en archivos ZIP.
- **Aislamiento del modo portable**: Prevenir que una instancia portable contamine el registro de Windows o `%LOCALAPPDATA%` al correr accidentalmente un instalador Inno Setup.

### 1.2 Cómo Estaba (Estado Previo)
- **Generación de artefactos en Windows (`setup/create_setup.cmd`)**:
  - Compilaba el instalador Inno Setup (`StremioSetup-v*-x64.exe`).
  - Creaba un archivo ZIP llamado `stremio-native-v%APP_VERSION%-x86_64-pc-windows-msvc.zip` que empaquetaba **únicamente** el ejecutable `stremio-installer.exe` (el instalador renombrado dentro de una carpeta temporal).
- **Publicación en GitHub Releases (`.github/workflows/release.yml`)**:
  - Se subía el archivo `stremio-native-v*-x86_64-pc-windows-msvc.zip`. Los usuarios que buscaban una versión portable descargaban dicho ZIP y al descomprimirlo encontraban únicamente el instalador (el "actualizador"), causando quejas y confusión.
- **Autoupdate (`app/src/updater.rs`)**:
  - Utilizaba la crate `self_update` para buscar un asset con nombre compatible con el target Rust (`x86_64-pc-windows-msvc.zip`), obligando a mantener ese ZIP artificial en las publicaciones.
  - Varios archivos aún conservaban el nombre y referencias al antiguo proyecto "Stremio Native" en accesos directos, AppUserModelID y metadatos del instalador.
  - Al ejecutar el instalador descargado mediante `std::process::Command::new(installer).args(["/SILENT", "/CLOSEAPPLICATIONS"])`, el proceso hijo compartía el árbol de procesos, con riesgo de deadlock si `stremio-oxide.exe` aún retenía handles de archivos abiertos o librerías dinámicas (`libmpv-2.dll`).

### 1.3 Cómo Va a Quedar (Estado Objetivo)
- **Publicación de binarios clara y diferenciada**:
  1. `StremioOxideSetup-v<version>-x64.exe` / `arm64.exe`: Instalador Inno Setup para instalación en el sistema con accesos directos actualizados (`Stremio Oxide`).
  2. `stremio-oxide-v<version>-windows-x64.zip` / `arm64.zip`: Paquete **verdaderamente portable**, que contiene la estructura completa:
     - `stremio-oxide.exe`
     - `libmpv-2.dll`
     - `msvc-runtime/*.dll`
     - `licenses/*`
     - Archivo marcador vacío `.portable` en el directorio raíz.
- **Eliminación del archivo engañoso**:
  - Se suprime por completo la creación de `stremio-native-v*-x86_64-pc-windows-msvc.zip`. No se sube ningún archivo denominado como actualizador.
- **Autoupdate directo (`app/src/updater.rs`)**:
  - El actualizador nativo identifica directamente el asset `StremioOxideSetup-v*-<arch>.exe` publicado en el release de GitHub (`stickyfran/stremio-oxide`).
  - Descarga dicho `.exe` al directorio local de staging (`updates/<version>/stremio-installer.exe`).

### 1.4 Guardrails y Edge Cases Críticos
> [!IMPORTANT]
> **Detección de Modo Portable**:
> Antes de verificar o descargar actualizaciones, `updater.rs` debe comprobar la existencia del archivo `.portable` junto al ejecutable actual (`std::env::current_exe()?.parent().join(".portable")`) o verificar si la ruta no pertenece a `%LOCALAPPDATA%\Programs\stremio-oxide`. Si la app se ejecuta en modo portable:
> - **Queda estrictamente deshabilitada la descarga y ejecución de `StremioOxideSetup.exe`**.
> - En la UI de Ajustes, el botón de actualización debe redirigir al release de GitHub (`open::that(release_page_url)`) para descargar el nuevo archivo `.zip` portable, o descargar el `.zip` para sobrescribir archivos in-place. Bajo ninguna circunstancia debe ejecutarse el instalador en modo portable para evitar ensuciar el registro de Windows.

> [!WARNING]
> **Prevención de Self-Deadlock en Windows (Detached Process & Handles)**:
> Cuando el instalador Inno Setup se invoque desde la app instalada:
> 1. Debe lanzarse utilizando la bandera de creación de proceso desacoplado de Win32:
>    `windows_sys::Win32::System::Threading::DETACHED_PROCESS` (vía `std::os::windows::process::CommandExt::creation_flags(0x00000008)`).
> 2. Parámetros recomendados: `["/SP-", "/SILENT", "/SUPPRESSMSGBOXES", "/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"]`.
> 3. El proceso padre `stremio-oxide.exe` debe ejecutar inmediatamente `std::process::exit(0)` tras despachar la creación del hijo, liberando al instante los handles sobre `stremio-oxide.exe` y `libmpv-2.dll` antes de que el instalador intente sobrescribirlos.

---

## 2. Descubrir y Biblioteca: Panel Lateral de Detalles (Preview Drawer)

### 2.1 Objetivos del Cambio
- **Cero latencia percibida (Zero-Latency UI Feedback)**: Abrir el panel de detalles de manera instantánea (0 ms) en el mismo frame en que el usuario hace click sobre una tarjeta.
- **Consistencia de interfaz**: Homogeneizar la experiencia entre Discover y Library para que el click simple inspeccione el elemento en el drawer lateral y el doble click / acción explícita navegue al detalle extendido.
- **Inmunidad ante condiciones de carrera (Race Condition Protection)**: Prevenir que respuestas asíncronas lentas de elementos cliqueados previamente sobrescriban la información del elemento activo actual.

### 2.2 Cómo Estaba (Estado Previo)
- **Condición en Slint (`app/ui/pages/discover.slint` y `app/ui/pages/library.slint`)**:
  ```slint
  is-open: root.has-preview && root.preview-title != "";
  width: root.is-open ? root.drawer-width : 0px;
  ```
- **Flujo en Rust (`app/src/models/discover.rs:321` y `app/src/models/library.rs:253`)**:
  1. El usuario hace click sobre una tarjeta de película o serie en el grid.
  2. Se invoca `item-selected(id, media-type, video-id)`.
  3. Se despacha `NavigationIntent::SelectDiscoverPreview` y se llama a `load_meta_details_for_video(&runtime, id, ...)`.
  4. Durante el tiempo que tarda la red y los addons en responder (entre 300 ms y 2000 ms), `has_preview` es `false` y `preview_title` es `""`.
  5. **Consecuencia visual**: La interfaz no realiza ningún cambio visible. La barra lateral permanece con ancho `0px`. El usuario asume que el click falló y vuelve a cliquear repetidamente.
  6. En Library, si `ui.get_library_preview_enabled()` está en `false`, el click salta directamente a la pantalla completa de detalles (`open_details_route`), expulsando al usuario de la vista de biblioteca.

### 2.3 Cómo Va a Quedar (Estado Objetivo)
- **Apertura Optimista Inmediata**:
  1. En el mismo instante del click, el componente `MediaCard` ya dispone de los datos locales del ítem (`MediaCardItem` contiene `title`, `poster`, `description`, `media_type` e `id`).
  2. El manejador de eventos asigna de inmediato estos campos a las propiedades del drawer:
     ```rust
     ui.set_discover_preview_id(card.id.clone().into());
     ui.set_discover_preview_title(card.title.into());
     ui.set_discover_preview_poster(card.poster);
     ui.set_discover_preview_description(card.description.into());
     ui.set_discover_has_preview(true);
     ui.set_discover_preview_ratings_loading(true);
     ```
  3. El panel derecho se expande inmediatamente (animación de 120 ms en 60 FPS) mostrando el póster, título y sinopsis básica, junto a un estado de carga (*skeleton/spinner*) para los datos remotos (calificaciones IMDb/RT/Metacritic y streams de trailer).
  4. Cuando `stremio-core` completa la respuesta de los addons, se actualizan las propiedades secundarias sin saltos de diseño (*Zero Cumulative Layout Shift*).
- **Interacción estandarizada en Library**:
  - Click simple: Despliega el Preview Drawer a la derecha para inspección rápida sin perder la posición del scroll de la biblioteca.
  - Botón "Ver Detalles Completos / Reproducir": Navega a la ruta detallada o inicia la reproducción.

### 2.4 Guardrails y Edge Cases Críticos
> [!CAUTION]
> **Condición de Carrera por Clicks Rápidos (Out-of-Order Async Responses)**:
> Si el usuario hace click en la Película A (cuya petición tarda 1500 ms) y 200 ms después hace click en la Película B (cuya petición tarda 300 ms), la respuesta de B llegará primero. Si a los 1500 ms llega la respuesta retardada de A, **sobrescribirá erróneamente los ratings, trailer y reparto de B**.
> 
> **Regla de Implementación**:
> Toda función que sincronice la respuesta de metadatos con la interfaz gráfica (`sync_preview_panel` en `details.rs` y callbacks en `ratings.rs`) **DEBE** validar la identidad del medio antes de mutar el modelo en Slint:
> ```rust
> let active_id = ui.get_discover_preview_id(); // o get_library_preview_id()
> if active_id.as_str() != incoming_meta_id {
>     tracing::debug!(%incoming_meta_id, %active_id, "Descartando metadatos obsoletos por click posterior");
>     return;
> }
> ```
> Opcionalmente, cancelar tareas asíncronas previas mediante un `tokio_util::sync::CancellationToken` asignado a cada cambio de selección.

---

## 3. Fin de Episodio y Transición a Siguiente Capítulo

### 3.1 Objetivos del Cambio
- **Continuidad de reproducción (*Binge-Watching Experience*)**: Eliminar las pantallas negras al finalizar un capítulo y garantizar una transición sin fricción al siguiente episodio de una serie.
- **Prevención de cierres de ventana no deseados**: Evitar que el reproductor libere las instancias gráficas y expulse al usuario a la pantalla de selección cuando se solicita el siguiente capítulo.
- **Resiliencia ante fallos de stream**: Manejar con elegancia la falta de streams o fallos de red en el siguiente capítulo sin dejar congelada la aplicación.

### 3.2 Cómo Estaba (Estado Previo)
- **Fin de archivo EOF (`app/src/mpv_integration.rs:3368`)**:
  ```rust
  PlaybackEvent::Ended { reason, error } => {
      if reason == EndReason::Eof {
          dispatch_player(core, ActionPlayer::Ended);
          let binge = model.ctx.profile.settings.binge_watching;
          let advanced = binge && play_next(core);
          if !advanced {
              // BLOQUE VACÍO: No se despliega ninguna UI.
          }
      }
  }
  ```
  Si `binge_watching` está desactivado o si `play_next` no cuenta con un stream ya resuelto (`next_stream == None`), el reproductor se detiene y la pantalla queda negra de forma permanente.
- **Comportamiento del botón manual "Siguiente Capítulo" (`app/src/mpv_integration.rs:2661`)**:
  ```rust
  if play_next(&core) { return; }
  ...
  if let (Some(ui), Some(saved_stream)) = (weak.upgrade(), saved_stream) {
      crate::deep_link::open_saved_stream(&ui, &core, &navigation, saved_stream);
      return;
  }
  unload_player(&controller, &core, ...); // ¡DESCARGA EL REPRODUCTOR!
  navigation.dispatch_and_project(&ui, NavigationIntent::OpenDetails { media_id: meta_id });
  ```
  Para cualquier episodio que el usuario no haya reproducido previamente, `saved_stream` es `None`. El código actual ejecuta `unload_player`, destruye la sesión de video de MPV y redirige al usuario a la pantalla de detalles para que vuelva a elegir un stream manualmente.

### 3.3 Cómo Va a Quedar (Estado Objetivo)
- **Overlay de fin de episodio (`NextEpisodeOverlay`)**:
  - Cuando el tiempo restante sea menor a 40 segundos, o cuando TheIntroDB / AniSkip detecten los créditos finales, o al alcanzar `EndReason::Eof`:
    - Aparece un cuadro en la esquina inferior derecha del reproductor con el título del siguiente episodio, miniatura y un botón "Reproducir Siguiente".
    - Si el modo maratón está habilitado, incluye una barra de cuenta regresiva de 10 segundos antes de avanzar automáticamente.
- **Resolución en caliente del siguiente stream (Hot Stream Switch)**:
  - Al presionar "Siguiente" o finalizar la cuenta regresiva:
    - **No se descarga el reproductor**. Se mantiene activa la superficie gráfica de MPV.
    - Se muestra el indicador de carga en pantalla: `"Cargando Episodio Siguiente..."`.
    - Se solicita la lista de streams para `next_video.id` a través de `stremio-core`.
    - Se envía el comando `loadfile <url>` a MPV, logrando una transición instantánea y continua entre capítulos.

### 3.4 Guardrails y Edge Cases Críticos
> [!IMPORTANT]
> **Heurística de Emparejamiento Oficial (`Stream.behaviorHints.bingeGroup`)**:
> No se deben implementar parsers frágiles ni expresiones regulares sobre los nombres de torrents para deducir el siguiente capítulo. `stremio-core` expone formalmente la propiedad:
> `stream.behavior_hints.binge_group: Option<String>`
> - Al resolver streams para el nuevo episodio, se debe priorizar aquel stream cuyo `binge_group` sea idéntico al del episodio actual.
> - Si ningún stream posee un `binge_group` coincidente, se utilizará el algoritmo de ranking configurado por el usuario (`StreamRanking::Smart` / `Quality`).

> [!WARNING]
> **Manejo de Timeouts y Fallo de Streams en Caliente**:
> La resolución en caliente no puede quedarse indefinidamente en estado de carga:
> 1. **Timeout estricto de 12 segundos**: Si los addons no devuelven streams compatibles o la red falla tras 12 segundos, se debe cancelar la espera.
> 2. **Fallback visual dentro del reproductor**:
>    - **NO** llamar a `unload_player` de forma abrupta.
>    - Abrir el panel lateral de selección de streams en el reproductor (drawer de streams) mostrando un mensaje informativo: *"No se pudo continuar automáticamente con la misma fuente. Elige un stream alternativo:"*.
>    - Permitir al usuario seleccionar manualmente otra fuente sin perder el estado fullscreen ni reiniciar la ventana de la aplicación.

---

## 4. Selección de Streams, Descargas y Expansión de Columna

### 4.1 Objetivos del Cambio
- **Legibilidad de metadatos de stream**: Permitir la lectura completa de nombres de archivo de torrents, calidades de video (4K HDR, Dolby Vision) y códecs de audio sin truncamiento agresivo.
- **Capacidad de descarga directa y fuera de línea**: Permitir la persistencia local de streams individuales o temporadas completas de series con un solo click.
- **Protección contra saturación y baneo de API (Rate-Limiting)**: Garantizar que la descarga masiva de capítulos no dispare bloqueos HTTP 429 por parte de proveedores Debrid ni caducidad de enlaces firmados.

### 4.2 Cómo Estaba (Estado Previo)
- **Ancho del panel en Slint (`app/ui/pages/details.slint:130`)**:
  - `property <length> drawer-width: 30 * root.unit;` (aproximadamente 480 px).
  - Los nombres técnicos de streams extensos (e.g., `Movie.Title.2024.2160p.UHD.Remux.HEVC.DV.TrueHD.7.1.Atmos-GROUP`) se cortaban a la mitad.
- **Fila de Stream (`app/ui/components/media_rows.slint:137`)**:
  - Cada fila solo disponía de un botón circular de reproducción (`play-button`).
  - No existía ninguna acción para descargar el contenido sin reproducirlo de inmediato.
  - La infraestructura de `DownloadManager` (`app/src/downloads.rs`) estaba aislada y solo se consumía desde la pantalla de descargas.

### 4.3 Cómo Va a Quedar (Estado Objetivo)
- **Panel ampliado un 30%**:
  - `drawer-width: 39 * root.unit;` (aproximadamente 624 px).
  - El espacio adicional permite visualizar el nombre completo del release, resolución, bitrate, número de seeders y proveedor sin superposiciones.
- **Botón doble: Descarga + Reproducción en `StreamRow`**:
  - Cada fila de stream presentará dos botones independientes en su lateral derecho:
    1. **Botón de Descarga** (icono de descarga / flecha hacia abajo).
    2. **Botón de Reproducir** (icono de play a la derecha).
  - Al hacer click en Descargar:
    - Se extrae la URL y metadatos del stream.
    - Se añade la tarea a `DownloadManager::enqueue`.
    - La descarga se refleja en tiempo real en la pestaña "Downloads" con control de pausa, reanudación y velocidad.
- **Descarga por lotes para Series**:
  - Si el elemento es una serie, al pulsar el botón de descarga se abre un modal de confirmación:
    - `"Descargar solo este capítulo"`
    - `"Descargar temporada completa (capítulos restantes)"`

### 4.4 Guardrails y Edge Cases Críticos
> [!CAUTION]
> **Resolución Just-In-Time (JIT) vs. Ráfagas Masivas a Addons/Debrid**:
> Si una temporada tiene 24 episodios y la aplicación intenta resolver simultáneamente los 24 streams al presionar "Descargar temporada":
> 1. **Baneo de IP / Rate Limit (HTTP 429)**: Servicios como Real-Debrid, AllDebrid o Torbox bloquearán las peticiones por exceso de llamadas en ráfaga.
> 2. **Caducidad de Enlaces Firmados**: Las URLs generadas por proveedores Debrid tienen un TTL estricto (generalmente entre 1 y 4 horas). Los enlaces de los últimos capítulos expirarían mucho antes de que la cola de descarga alcance esos archivos.
> 
> **Regla de Implementación**:
> La cola de descargas por lotes **NO** debe almacenar URLs definitivas para toda la temporada.
> Debe guardar un registro de intención de descarga diferida:
> ```rust
> pub struct DeferredBatchItem {
>     pub series_id: String,
>     pub season: i32,
>     pub episode: i32,
>     pub preferred_binge_group: Option<String>,
>     pub fallback_addon_transport_url: Option<String>,
> }
> ```
> La resolución del stream del capítulo $N+1$ debe ejecutarse **Just-In-Time (JIT)** únicamente cuando la descarga del capítulo $N$ alcance el 95% o termine, manteniendo una concurrencia máxima de resolución de 1 o 2 elementos activos.

---

## 5. Barra de Buffer y Optimización de Caché MPV

### 5.1 Objetivos del Cambio
- **Precisión visual**: Reflejar exactamente cuántos segundos de video se encuentran en la memoria RAM del demuxer.
- **Navegación instantánea (Instant Seeking)**: Permitir avanzar y retroceder sin interrupciones ni tiempos de espera cuando los datos ya han sido precargados.
- **Estabilidad de memoria y respeto a swarms P2P**: Evitar que el lector de caché sature la memoria RAM del sistema ni asfixie la descarga secuencial del motor torrent local.

### 5.2 Cómo Estaba (Estado Previo)
- **Extracción de la propiedad en `playback-mpv/src/actor.rs:1774`**:
  ```rust
  "demuxer-cache-time" => {
      state.buffered_until = property_double(property)...
  }
  ```
- **Cálculo porcentual erróneo en `app/src/mpv_integration.rs:4516`**:
  ```rust
  let buffered_percent = if state.duration > 0.0 {
      ((state.buffered_until / state.duration) * 100.0).clamp(0.0, 100.0) as f32
  } else {
      0.0
  };
  ```
- **Error matemático detectado**:
  - En la especificación de MPV, `demuxer-cache-time` entrega los **segundos relativos almacenados por delante del cabezal de reproducción**, no la marca de tiempo absoluta en el archivo.
  - Si una película dura 2 horas (7200 s) y el usuario va por el minuto 60 (3600 s) con 120 segundos de buffer por delante, `state.buffered_until` vale `120.0`.
  - El cálculo arrojaba `(120 / 7200) * 100 = 1.6%`.
  - La interfaz graficaba la barra de buffer al inicio absoluto de la barra de progreso (en el primer 1.6%), totalmente desvinculada de la posición actual del usuario (50%).

### 5.3 Cómo Va a Quedar (Estado Objetivo)
- **Fórmula de buffer matemáticamente exacta**:
  ```rust
  let current_pos = state.position.max(0.0);
  let buffered_end = current_pos + state.buffered_until;
  let buffered_percent = if state.duration > 0.0 {
      ((buffered_end / state.duration) * 100.0).clamp(0.0, 100.0) as f32
  } else {
      0.0
  };
  ```
  La barra gris de precarga se extenderá desde el cabezal de reproducción hacia adelante de forma coherente y continua.
- **Configuración de memoria de demuxer por perfil**:
  - Para contenido completamente descargado o local: Se marca el buffer al 100% y el salto en cualquier dirección es de 0 ms.
  - Para streams en red remotos (HTTP/HLS): Se configuran los límites optimizados de demuxer en MPV.

### 5.4 Guardrails y Edge Cases Críticos
> [!WARNING]
> **Saturación de RAM y Swarms Lentos de Torrents**:
> Aplicar parámetros de buffer universales e indiscriminados provoca dos fallas severas:
> 1. **Asfixia del motor torrent local (Streaming Server)**: Si el usuario reproduce un torrent P2P con pocos seeders y MPV solicita agresivamente 300 segundos hacia adelante a máxima velocidad mediante peticiones HTTP Range continuas, fragmenta las prioridades de descarga de piezas secuenciales del motor de torrents local, provocando micro-cortes en el punto de reproducción actual.
> 2. **Consumo desmedido en trailers**: Una instancia de previsualización no puede reservar 1 GB de memoria de demuxer para un video promocional de 2 minutos.
> 
> **Regla de Implementación**:
> Separar estrictamente los perfiles de configuración de MPV:
> - **Perfil Trailers (`preview_player`)**:
>   `demuxer-max-bytes=64MiB`, `demuxer-max-back-bytes=0MiB`, `demuxer-readahead-secs=15`.
> - **Perfil Torrent Local (`127.0.0.1`)**:
>   `demuxer-max-bytes=256MiB`, `demuxer-max-back-bytes=64MiB`, `demuxer-readahead-secs=60` (para permitir que el motor de streaming de Stremio gestione la estrategia secuencial de piezas).
> - **Perfil HTTP Directo / Debrid**:
>   `demuxer-max-bytes=512MiB` (hasta 1024 MiB en PC con >16 GB RAM), `demuxer-max-back-bytes=128MiB`, `demuxer-readahead-secs=180`.

---

## 6. Metadatos de Episodios y Botón de Trailer en Pantalla Gigante

### 6.1 Objetivos del Cambio
- **Contexto narrativo de episodios**: Presentar la sinopsis de cada capítulo en los listados de temporadas antes de iniciar la visualización.
- **Acceso consistente al trailer**: Asegurar que el botón para reproducir el trailer en la vista detallada de pantalla completa nunca desaparezca de manera imprevista.

### 6.2 Cómo Estaba (Estado Previo)
- **Listado de episodios (`app/ui/pages/details.slint:28`)**:
  - El struct `EpisodeItem` carecía del campo `description`.
  - Solo contenía `id`, `title`, `released`, `thumbnail`, `season`, `episode-num`, `is-upcoming`, `is-watched`, `is-scheduled`, `progress`, `can-play`.
  - Los usuarios solo veían el título del capítulo y la miniatura, sin saber de qué trataba el episodio.
- **Botón de trailer en pantalla completa (`app/src/event_loop.rs:728`)**:
  ```rust
  let trailer_selection_id = details_stream_sync_needed
      .then(|| playback_selections.trailer_id())
      .flatten();
  ...
  ui.set_detail_trailer_selection_id(
      trailer_selection_id.as_deref().unwrap_or_default().into(),
  );
  ```
  - `details_stream_sync_needed` solo es `true` mientras los addons de streams están respondiendo. En cuanto la lista de streams se estabiliza, pasa a ser `false`.
  - En el siguiente ciclo del event loop, `trailer_selection_id` pasaba a ser `None`, seteando `trailer-selection-id = ""` en Slint.
  - La condición de visibilidad en `details.slint:237`:
    ```slint
    show-trailer: root.has-trailer && root.trailer-selection-id != "";
    ```
    se evaluaba como falsa y el botón de "Ver Trailer" desaparecía de la pantalla del usuario.

### 6.3 Cómo Va a Quedar (Estado Objetivo)
- **Soporte de descripción de episodios**:
  1. Se incorpora `description: string` en `EpisodeItem` en Slint.
  2. En `app/src/models/details.rs:1319`, se extrae el texto desde `video.overview` (disponible en `stremio_core::types::resource::Video`):
     ```rust
     description: video.overview.as_deref().unwrap_or_default().into(),
     ```
  3. En `EpisodeRow` (`app/ui/components/media_rows.slint`), se incluye un bloque tipográfico secundario que muestra la sinopsis debajo del título del capítulo con opción de truncado inteligente.
- **Persistencia garantizada del Trailer**:
  - `trailer_selection_id` se resuelve directamente a partir de `meta_item.preview.trailer_streams` sin depender de la variable de estado efímera `details_stream_sync_needed`.
  - El botón de trailer permanece siempre visible y habilitado mientras la película o serie cuente con un trailer disponible.

---

## 7. Optimización de Inicio de Trailer (*Fast Startup*)

### 7.1 Objetivos del Cambio
- **Reducción drástica del tiempo de respuesta**: Bajar la latencia de apertura del trailer de 3–5 segundos a menos de 200 milisegundos.
- **Eliminación del Cold-Start de procesos**: Evitar instanciaciones en frío de entornos de ejecución de Python durante la navegación interactiva.
- **Resiliencia ante contramedidas de YouTube**: Evitar bloqueos HTTP 403 y fallos de descifrado `n-sig` en streams de YouTube.

### 7.2 Cómo Estaba (Estado Previo)
- **Resolución mediante YouTube (`app/src/playback.rs:500`)**:
  ```rust
  if let StreamSource::YouTube { yt_id } = &stream.source {
      return Some(format!("https://www.youtube.com/watch?v={yt_id}"));
  }
  ```
- **Cadena de ejecución de MPV**:
  1. Al recibir la URL estándar de YouTube, MPV delega la extracción a `yt-dlp`.
  2. `yt-dlp` arranca un proceso en frío de Python (en Windows suele ser un ejecutable empaquetado con PyInstaller que se descomprime en `%TEMP%`, demorando por sí solo entre 600 ms y 1.2 segundos).
  3. Realiza la descarga del HTML de la página web de YouTube.
  4. Ejecuta un motor de JavaScript para resolver las firmas y el reto `n-sig`.
  5. Extrae los formatos adaptativos de video y audio DASH.
  6. **Penalización**: El proceso toma entre 2.5 y 5.0 segundos de espera con pantalla congelada antes de mostrar el primer frame del video.

### 7.3 Cómo Va a Quedar (Estado Objetivo)
1. **Prefetching Asíncrono en segundo plano**:
   - En el momento en que el usuario abre el Preview Drawer o visualiza la pantalla de detalles de un medio, un worker en segundo plano con Tokio inicia la resolución anticipada de la URL directa del trailer.
   - El resultado se almacena en una caché en memoria en Rust (`TrailerUrlCache`) vinculada al `yt_id`, con un TTL de 2 horas.
2. **Reproducción con URL Directa Resuelta (`ytdl=no`)**:
   - Al hacer click en "Ver Trailer", si la URL de `googlevideo.com` ya se encuentra en caché:
     - Se pasa directamente dicha URL resuelta a libmpv configurando la opción `ytdl=no`.
     - MPV no invoca ningún proceso externo ni hook de Python; simplemente abre la conexión HTTP directa al video.
     - **Tiempo de inicio real alcanzado: ~120 ms a 180 ms**.

### 7.4 Guardrails y Edge Cases Críticos
> [!CAUTION]
> **Inestabilidad del cliente `android` y Selección de Formatos Pre-muxados**:
> 1. **Bloqueos recientes de YouTube a `player_client=android`**: En cambios recientes de infraestructura, YouTube comenzó a rechazar peticiones del cliente Android que no suministran *Proof of Origin Tokens* (PO Tokens), arrojando errores HTTP 403 Forbidden.
>    - **Regla**: En los argumentos del extractor de `yt-dlp` debe especificarse la cadena de clientes estables recomendada:
>      `--extractor-args "youtube:player_client=ios,web_embedded"`
> 2. **Evitar streams separados (Split Video/Audio)**: Si `yt-dlp` resuelve un stream de video DASH y un stream de audio separado, MPV se ve forzado a abrir dos conexiones HTTP paralelas y sincronizarlas por software.
>    - **Regla**: El comando de prefetching debe solicitar un formato pre-muxado en un único contenedor a 720p (por ejemplo formato MP4 progresivo o stream HLS):
>      `yt-dlp --get-url -f "best[height<=720][protocol^=http]/best[height<=720]"`
> 3. **Fallback si el prefetch aún está en vuelo**: Si el usuario presiona "Ver Trailer" en menos de 200 ms tras abrir el drawer (antes de que el worker termine):
>    - La UI debe mostrar un indicador de carga mientras espera por el `tokio::sync::oneshot::Receiver` de la tarea en vuelo, en lugar de lanzar una segunda instancia concurrente de `yt-dlp`.

---

## 8. Matriz Comparativa para Auditoría

| Área / Característica | Cómo Estaba (Previo) | Cómo Va a Quedar (Objetivo) | Restricción Crítica / Guardrail (Auditoría) |
| :--- | :--- | :--- | :--- |
| **Windows Release y Portable** | ZIP con instalador renombrado dentro (`stremio-installer.exe`). | Instalador `.exe` + ZIP portable real con todas las dependencias y archivo `.portable`. | Si existe `.portable`, deshabilitar autoupdate Inno Setup. Desacoplar proceso con `DETACHED_PROCESS` y `exit(0)`. |
| **Nombre de Aplicación** | Referencias residuales a "Stremio Native" en UI y accesos. | Nombre 100% unificado como "Stremio Oxide". | Actualizar AppUserModelID a `com.stremio.oxide` para no corromper la barra de tareas. |
| **Drawer en Discover / Library** | Ancho 0 px hasta que la red responde (200–2000 ms). En Library saltaba a pantalla completa. | Apertura en 0 ms con datos locales de la tarjeta y skeleton de carga. Click simple inspecciona en drawer. | **Validar coincidencia de ID**: `incoming_id == active_preview_id` para evitar race conditions por clicks rápidos. |
| **Fin de Episodio (EOF)** | Pantalla negra congelada; botón de siguiente expulsaba del reproductor. | Overlay flotante con cuenta regresiva; resolución de stream en caliente sin cerrar MPV. | Utilizar `Stream.behaviorHints.bingeGroup`. Timeout estricto de 12s con fallback al drawer de streams sin cerrar MPV. |
| **Descargas en Streams** | Solo botón de reproducción; ancho de panel ajustado a 30 units. | Botón de descarga junto a Play; soporte de temporada completa; panel ampliado a 39 units (+30%). | **Resolución JIT secuencial**: Nunca disparar 24 resoluciones simultáneas para evitar rate-limits (HTTP 429) y expiración de URLs. |
| **Barra de Buffer** | Cálculo porcentual dividía segundos relativos por duración total (dibujaba ~1%). | Fórmula absoluta corregida (`pos + cache_time`) y caché configurado por perfiles. | Perfil específico de demuxer para Torrents locales (256MB / 60s) para no asfixiar la descarga secuencial del swarm. |
| **Metadatos y Trailer** | Sinopsis de episodios ausente; botón de trailer desaparecía al finalizar sync de streams. | Sinopsis mapeada desde `Video.overview`; persistencia fija de `trailer_selection_id`. | No ligar la visibilidad del botón de trailer al flag transitorio `details_stream_sync_needed`. |
| **Velocidad de Trailers** | 2.5 a 5 segundos de espera por descifrado JS de YouTube y cold start de Python. | Menos de 200 ms mediante prefetching asíncrono en caché y reproducción directa con `ytdl=no`. | Usar clientes `ios,web_embedded` para evitar bloqueos HTTP 403; solicitar streams pre-muxados a 720p. |
