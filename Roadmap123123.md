# Roadmap de Optimización de Rendimiento, Inicio Ultrarrápido y Cero-Flicker

Este documento detalla la arquitectura, las fases ya implementadas, y las fases planificadas para maximizar la velocidad de apertura (*cold-start*), eliminar *glitches* visuales en el ciclo de vida de autenticación y optimizar el consumo de recursos (CPU/RAM/I/O) en **Stremio Native / Oxide**.

Esta versión incorpora las observaciones y recomendaciones técnicas de la **auditoría de arquitectura de producción** (manejo de contención SQLite, deserialización en background, barreras de sincronización en streaming temprano, diffing quirúrgico en Slint, calentamiento *post-render idle* y telemetría de regresiones).

---

## Índice

1. [Visión General y Objetivos](#1-visión-general-y-objetivos)
2. [Fase 1: Eliminación del Glitch Visual de Login (Zero-Flash Auth) — *Completada*](#2-fase-1-eliminación-del-glitch-visual-de-login-zero-flash-auth--completada)
3. [Fase 2: Batching SQL, Memory-Mapped I/O y Deserialización en Background](#3-fase-2-batching-sql-memory-mapped-io-y-deserialización-en-background)
4. [Fase 3: Desacoplamiento del Servidor de Streaming y Barrera de Reproducción Temprana](#4-fase-3-desacoplamiento-del-servidor-de-streaming-y-barrera-de-reproducción-temprana)
5. [Fase 4: Caché Instantánea de Vista de Arranque (SWR) con Diffing Quirúrgico en Slint](#5-fase-4-caché-instantánea-de-vista-de-arranque-swr-con-diffing-quirúrgico-en-slint)
6. [Fase 5: Inicialización Escalonada en Post-Render Idle (MPV, Shaders y RPC)](#6-fase-5-inicialización-escalonada-en-post-render-idle-mpv-shaders-y-rpc)
7. [Fase 6: Telemetría Estructurada de Arranque y CI Anti-Regresiones](#7-fase-6-telemetría-estructurada-de-arranque-y-ci-anti-regresiones)
8. [Matriz de Impacto, Casos Borde y Métricas Clave](#8-matriz-de-impacto-casos-borde-y-métricas-clave)

---

## 1. Visión General y Objetivos

### Objetivos Principales:
* **Percepción de Cero Latencia (Instant Cold-Start)**: Lograr que la interfaz sea visualmente rica e interactiva en menos de **150-200 ms** desde la invocación del ejecutable.
* **Integridad Visual Absoluta (Zero Glitches)**: Ningún parpadeo o flash de interfaces intermedias (p. ej. pantalla de login para usuarios con sesión activa, o parpadeo por reemplazo tosco de modelos en Slint).
* **Eficiencia de I/O y Concurrencia Real**: Eliminar la falsa concurrencia sobre recursos compartidos (como el mutex de conexión de base de datos) y aprovechar *memory-mapped I/O*.
* **Seguridad y Resiliencia**: Tolerar clics tempranos del usuario en el milisegundo 100 sin errores de *Connection Refused*, y manejar de forma reactiva puertos dinámicos.

---

## 2. Fase 1: Eliminación del Glitch Visual de Login (Zero-Flash Auth) — *Completada*

### Diagnóstico Inicial:
Al arrancar, `username` en Slint nacía con valor vacío (`""`). Como la regla de visibilidad era `show-auth-page: root.username == ""`, al ejecutarse `ui.show()?` la GPU dibujaba la pantalla de autenticación (`AuthPage`). Tras resolver la sesión en Tokio, se inyectaba el email real y la pantalla de login desaparecía de golpe.

### Implementación y Auditoría:
1. **Desacoplamiento en Slint (`app/ui/app.slint`)**:
   * Se introdujo `in-out property <bool> auth-initialized: false;`.
   * Se blindaron las propiedades derivadas:
     ```slint
     private property <bool> show-auth-page: root.debug-preview-enabled
         ? root.debug-preview-page == -1
         : (root.auth-initialized && root.username == "");
     private property <bool> show-main-shell: root.debug-preview-enabled
         ? (root.debug-preview-page >= 0 && root.debug-preview-page <= 6) || root.debug-preview-page == 9 || root.debug-preview-page == 10
         : (!root.auth-initialized || root.username != "");
     ```
   * **Garantía**: Durante el arranque inicial (`auth-initialized = false`), la pantalla de login **jamás** se renderiza. La aplicación presenta la interfaz principal con los placeholders de carga correspondientes.

2. **Pre-hidratación Inmediata en Rust (`app/src/main.rs`)**:
   * Funciones `load_early_auth_state()` y `extract_auth_from_profile_json()` ejecutadas justo tras inicializar SQLite y **antes** de `ui.show()?`.
   * Lectura sub-milisegundo (< 0.2 ms) de `profile_core_storage`.
   * Inyección de `username`, `avatar_letter` y avatar en caché previo al primer fotograma.

3. **Sincronización en Runtime y Tests Unitarios**:
   * Sincronizado en `event_loop.rs` y `models/auth.rs` para flujos de login, logout y guest.
   * Tests unitarios añadidos en `main.rs::tests` verificando extracciones de JSON válidas, vacías, nulas y malformadas con 100% de éxito.

---

## 3. Fase 2: Batching SQL, Memory-Mapped I/O y Deserialización en Background

### Diagnóstico de Cuello de Botella:
En `load_startup_storage()`, un `tokio::join!` de 9 llamadas a `DesktopEnv::get_storage(...)` genera contención sobre un único `Mutex<Connection>`, serializando las 9 consultas y acumulando sobrecarga de *syscalls* de lectura. Además, en bibliotecas grandes, deserializar el bucket `library` en el hilo de inicio puede introducir *jank*.

### Plan de Implementación e Incorporaciones Técnicas:
1. **Pragmas de Alto Rendimiento en SQLite**:
   * Asegurar en la conexión de arranque:
     ```sql
     PRAGMA journal_mode = WAL;
     PRAGMA synchronous = NORMAL;
     PRAGMA temp_store = MEMORY;
     PRAGMA mmap_size = 30000000; -- 30 MB de memory-mapped I/O para lecturas en frío instantáneas
     ```
2. **Consulta por Lotes (Batch Query)**:
   * Reemplazar las 9 consultas individuales por una única operación atómica:
     ```sql
     SELECT key, value FROM profile_core_storage 
     WHERE profile_id = ? 
       AND key IN ('profile', 'library_recent', 'library', 'streams', 'server_urls', 'notifications', 'search_history', 'dismissed_events');
     ```
3. **Deserialización No Bloqueante (Off-Thread)**:
   * El bucket `profile` se procesa prioritariamente para mantener activa la sesión.
   * Si el payload de `library` es grande (> 500 KB), mover la deserialización `serde_json::from_str` a `tokio::task::spawn_blocking` para no bloquear el hilo de Slint ni demorar la respuesta de la interfaz.

---

## 4. Fase 3: Desacoplamiento del Servidor de Streaming y Barrera de Reproducción Temprana

### Diagnóstico de Cuello de Botella:
El arranque de `stremio-runtime` (libtorrent, sockets DHT, UPnP, listeners HTTP) toma entre 80 ms y 400 ms. Bloquear `finish_startup()` con `server_task.await` frena innecesariamente la interactividad del catálogo.

### Plan de Implementación e Incorporaciones Técnicas:
1. **Arranque en Background Asíncrono**:
   * Pre-configurar en el modelo la URL determinista `http://127.0.0.1:{config.torrent_port}`.
   * `stremio_runtime::start` corre en un hilo independiente sin bloquear la creación del `Runtime` de Stremio.
2. **Barrera de Reproducción Temprana (Signal / Watch Barrier)**:
   * *Caso Borde*: Si el usuario hace clic en un contenido en los primeros 100 ms antes de que libtorrent levante el socket TCP, la petición fallaría con *Connection Refused*.
   * *Solución*: Implementar un canal `tokio::sync::watch` o `Arc<tokio::sync::Notify>` (`ServerReadiness`). Si se dispara una acción de reproducción mientras `ServerStatus == Starting`, el comando espera de forma silenciosa la señal de `Ready` (apenas unas decenas de milisegundos) antes de pasar la URL a `libmpv`, sin arrojar errores al usuario.
3. **Manejo de Fallback de Puerto Ocupado**:
   * Si el puerto configurado estaba en uso y el runtime cambia dinámicamente a otro puerto libre, se emite un evento interno reactivo que actualiza `streaming_server_urls` en `stremio-core` y en Slint.

---

## 5. Fase 4: Caché Instantánea de Vista de Arranque (SWR) con Diffing Quirúrgico en Slint

### Diagnóstico de Cuello de Botella:
Mostrar esqueletos de carga vacíos en el Board hasta que los addons remotos respondan por red da una sensación de lentitud ("app fría"), aunque el usuario haya abierto la app hace instantes.

### Plan de Implementación e Incorporaciones Técnicas:
1. **Persistencia de Snapshot en SQLite**:
   * Guardar al cerrar o en segundo plano un snapshot serializado de las filas activas del Board (`CatalogsWithExtra`) y de Continuar Viendo para el perfil activo.
2. **Poblado en Frame 0**:
   * Al iniciar, si el snapshot existe y es válido, se inyecta en los modelos de Slint antes de que la ventana complete su presentación.
3. **Diffing Quirúrgico en `VecModel` (Anti-Flicker)**:
   * *Caso Borde*: Cuando los addons responden por red 800 ms después, si se reemplaza todo el `VecModel` de Slint indiscriminadamente, la UI destruye nodos, reinicia la posición de scroll y resetea texturas de GPU, causando parpadeos.
   * *Solución*: Implementar diffing por fila calculando un hash rápido (`ahash` o `xxhash`) o comparando IDs de ítems. Si la fila es idéntica a la del snapshot, **no se muta la propiedad de Slint**. Solo se actualizan filas que presenten diferencias reales.
4. **Caché en Disco de Pósters y Artwork**:
   * Respaldar el cargador de imágenes de Slint con la caché local de disco de `image_cache`, garantizando que los pósters del snapshot se lean de disco local y no aparezcan en gris esperando a la red.

---

## 6. Fase 5: Inicialización Escalonada en Post-Render Idle (MPV, Shaders y RPC)

### Diagnóstico de Cuello de Botella:
Inicializar componentes pesados que no se usan en el segundo 0 (preparación de shaders Anime4K, verificación de scripts obsoletos de ThumbFast, handshake de Discord RPC) satura la CPU durante el arranque. Sin embargo, diferirlos hasta el primer clic en "Play" aumentaría el tiempo de inicio del reproductor.

### Plan de Implementación e Incorporaciones Técnicas:
1. **Calentamiento en Post-Render Idle (Ventana de 500ms - 1000ms)**:
   * Programar la llamada a `mpv_integration::prepare_playback_files()` en un hilo de baja prioridad **500 ms después** de que la ventana principal ya haya sido dibujada y el hilo de eventos esté ocioso.
   * De este modo, los shaders de Anime4K y carpetas de captura ya están preparados en la caché del sistema operativo antes de que el usuario elija qué reproducir, sin restar ni un milisegundo al arranque de la ventana.
2. **Diferimiento de Servicios Secundarios**:
   * Retrasar el handshake de Discord RPC y la búsqueda de updates del `updater` entre 1.5 y 2.5 segundos tras el arranque.

---

## 7. Fase 6: Telemetría Estructurada de Arranque y CI Anti-Regresiones

### Diagnóstico de Calidad:
Las estimaciones de arranque requieren mediciones empíricas automatizadas para que futuros commits no degraden silenciosamente la velocidad ganada.

### Plan de Implementación e Incorporaciones Técnicas:
1. **Instrumentación de Hitos de Arranque (Structured Tracing)**:
   * Medir y reportar en logs de depuración hitos exactos con `Instant::now()`:
     * `t_db_init`: Tiempo de apertura y pragmas de SQLite.
     * `t_early_auth`: Detección temprana de sesión.
     * `t_window_shown`: Momento exacto de emisión de `ui.show()?`.
     * `t_snapshot_painted`: Momento en que el Board previo se dibuja en pantalla.
     * `t_server_ready`: Momento en que el motor torrent reporta socket abierto.
2. **Pruebas Automatizadas de Rendimiento en CI**:
   * Alertas o aserciones de umbral que fallen si `t_window_shown` excede los 250 ms en condiciones estándar.

---

## 8. Matriz de Impacto, Casos Borde y Métricas Clave

| Fase | Área Técnica | Caso Borde Mitigado | Reducción de Latencia Estimada | Estado |
| :--- | :--- | :--- | :--- | :--- |
| **Fase 1** | Autenticación Slint + Rust | Glitch de login momentáneo para usuarios con sesión activa | Inmediato (Percepción de estabilidad) | **Completado** |
| **Fase 2** | SQLite + Serde I/O | Bloqueo de hilo por `library` pesada (>500KB) + contención de mutex | ~40 - 90 ms | *Planificado* |
| **Fase 3** | Servidor Torrent P2P | *Connection Refused* si usuario da "Play" en el primer segundo | ~100 - 350 ms | *Planificado* |
| **Fase 4** | Snapshot SWR + VecModel | Parpadeo y reinicio de scroll al llegar datos remotos (Diffing `ahash`) | Percepción de 0 ms (Pósters instantáneos) | *Planificado* |
| **Fase 5** | MPV Shaders & Discord RPC | Retardo de reproducción si se aplaza al "Play" (Solución: *Post-render idle*) | ~30 - 70 ms | *Planificado* |
| **Fase 6** | Telemetría & CI | Regresiones silenciosas de tiempo de inicio en futuros commits | Blindaje continuo | *Planificado* |
