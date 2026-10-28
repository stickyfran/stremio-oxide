# Auditoría Profunda — Stremio Native / Oxide

**Fecha:** 28 de octubre de 2026
**Árbol auditado:** `a020c81` + cambios locales sin commitear (working tree)
**Alcance:** workspace Rust completo (`app`, `core-env`, `credential-store`, `debrid`, `media-integrations`, `playback-mpv`, `server-connector`, `stream-ranking`, `stremio-runtime`), ~95 archivos `.rs`, ~58 000 líneas, más `ARCHITECTURE.md`, `RamStremio.md` y `Roadmap123123.md`.
**Método:** lectura estática dirigida, verificación cruzada de cada hallazgo contra el código real, comprobación de semántica de dependencias (Turso/Slint) y revisión de los documentos de diseño existentes. No se ejecutó la aplicación ni pruebas de carga; los hallazgos marcados como *requiere confirmación en runtime* están indicados explícitamente.

> **Nota sobre líneas:** los números de línea corresponden al árbol de trabajo en la fecha indicada. Pueden desplazarse con los cambios locales pendientes.
>
> **Nota sobre el tono:** este documento prioriza lo que puede romper, filtrar datos, congelar la UI o degradar el rendimiento. Los aciertos también se documentan al final (§11) para no convertir la auditoría en una lista de agravios.

---

## 1. Resumen ejecutivo

El proyecto tiene una base técnica sólida: separación real de hilos, proyección unidireccional, caché de imágenes acotada, debounce de escrituras en SQLite, diffing de modelos Slint y un pipeline de reproducción con FBO compartido. La mayoría de los problemas encontrados **no son de diseño general, sino de casos borde, concurrencia y confianza en la entrada**.

El veredicto es claro: **hay 6 problemas críticos que deben corregirse antes de cualquier distribución pública**. Uno de ellos es de seguridad y es explotable desde cualquier página web abierta en la misma máquina (el servidor de streaming escucha en `0.0.0.0` sin autenticación y con CORS abierto); uno puede destruir la configuración del usuario de forma silenciosa; uno permite que dos tareas intercalen transacciones sobre la misma conexión de base de datos; y tres afectan a la disponibilidad (fuga permanente de torrents, panic al manejar una petición IPv6 y pérdida total de actualizaciones de UI).

| # | Severidad | Área | Hallazgo | Archivo clave |
|---|---|---|---|---|
| SEC-01 | **Crítica** | Seguridad | API local sin auth en `0.0.0.0`, CORS `*`, mutaciones por GET, `path`/`from` arbitrarios | `stremio-runtime/src/lib.rs:808` |
| BUG-01 | **Crítica** | Runtime P2P | `active_streams` se incrementa en todas las peticiones y solo se decrementa en el body exitoso → torrents inmortales | `runtime/torrent_http.rs:238` |
| BUG-02 | **Crítica** | Persistencia | `turso::Connection::clone` comparte el mismo `Arc`; `core-env` lo usa sin mutex → transacciones intercaladas | `core-env/src/lib.rs:498` |
| BUG-03 | **Crítica** | Configuración | Un JSON corrupto/ilegible reemplaza toda la configuración por defaults | `app/src/config.rs:241` |
| BUG-04 | **Crítica** | Proxy | Un destino IPv6 provoca `expect()` y mata la tarea de la petición | `runtime/proxy.rs:27` |
| BUG-05 | **Crítica** | UI | `expect("model read failed")` en el bucle de proyección: un fallo transitorio detiene todas las actualizaciones de UI | `app/src/event_loop.rs:402` |
| SEC-03 | Alta | Actualizaciones | Instalador descargado sin firma/checksum y ejecutado desde un directorio escribible (TOCTOU) | `app/src/updater.rs:445` |
| BUG-14 | Alta | Reproducción | `loadfile` síncrono bloquea el actor MPV; la cola se llena y se pierden comandos/eventos | `playback-mpv/src/actor.rs:1301` |
| BUG-15 | Alta | FFI | El callback `wakeup` se desregistra solo en la salida feliz → posible UAF tras un panic | `playback-mpv/src/actor.rs:574` |
| PERF-01 | Alta | UI | Clonado profundo de submodelos completos en cada `NewState` | `app/src/event_loop.rs:596` |
| PERF-02 | Alta | GPU | Captura/restauración de ~24 `glGet*` por frame, dos veces por frame | `playback-mpv/src/render.rs:537` |
| PERF-03 | Alta | Miniaturas | 3 copias completas de frame + rotación píxel a píxel + escala CPU | `playback-mpv/src/thumbnail.rs:1183` |

**Recuento:** 6 críticos · 17 altos · 32 medios · 7 bajos.

---

## 2. Metodología y límites

### 2.1 Qué se revisó

- **Rutas de datos completas:** petición HTTP del addon → caché → `stremio-core` → proyección Slint; magnet → librqbit → stream HTTP por rangos; evento MPV → actor → `PlaybackState` → propiedad Slint.
- **Rutas de fallo:** timeouts, cancelaciones, errores de red, cierres de ventana, cambios de perfil y apagado.
- **Concurrencia:** locks `std::sync` vs `tokio::sync`, guards retenidos a través de `await`, orden de adquisición, canales acotados/no acotados.
- **FFI y GPU:** propiedad de punteros, callbacks `extern "C"`, ciclo de vida de texturas/FBO, estado OpenGL.
- **Persistencia:** WAL, transacciones, checkpoint, debounce, integridad de backups, esquema.
- **Seguridad:** superficie de red local, autenticación, CORS, path traversal, SSRF, permisos de archivos, cadena de actualización.

### 2.2 Qué no se hizo

- No se ejecutó `cargo test`, `clippy`, ni la app. No hay mediciones de CPU/GPU/RAM en vivo.
- No se auditó el código de `libmpv`, `librqbit`, `ffmpeg` ni `winit` más allá de su uso.
- No se revisó el código generado por Slint (`app/ui/`) línea a línea; sí su integración y el coste de proyección.
- No se evaluó la cadena de CI/CD más allá de lo que afecta al binario distribuido.

### 2.3 Cómo leer las severidades

| Severidad | Criterio |
|---|---|
| **Crítica** | Explotable remotamente, pérdida de datos irreversible, corrupción de estado, o caída funcional total sin recuperación. |
| **Alta** | Degradación severa, fuga de recursos a largo plazo, freeze perceptible, o bug funcional frecuente. |
| **Media** | Coste medible en CPU/IO/RAM, jank en escenarios concretos, o bug con workaround. |
| **Baja** | Robustez, mantenibilidad, documentación o mejoras incrementales. |

---

## 3. Hallazgos críticos

### SEC-01 · Crítica — El servidor de streaming es una API local sin autenticación, expuesta a la LAN y a cualquier web

**Archivo:** `stremio-runtime/src/lib.rs:808`, `:752-764`; `runtime/torrent_service.rs:22-46`; `runtime/torrent_http.rs:60-77`

**Qué ocurre.** El listener se enlaza siempre a `0.0.0.0`, incluso en modo monousuario:

```rust
// stremio-runtime/src/lib.rs:808
let bind_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
```

La URL que se anuncia es `127.0.0.1`, lo que oculta que el socket acepta conexiones de toda la LAN. Además:

```rust
// stremio-runtime/src/lib.rs:752
let cors = CorsLayer::new()
    .allow_origin(Any)
    .allow_methods([Method::GET, Method::POST, ...])
    .allow_headers(Any);
```

No hay token, `Origin` allow-list, ni comprobación de `Host`. Las rutas mutantes son **GET**: `/removeAll`, `/{hash}/remove`, `/{server_key}/downloader/*`. `POST /create` acepta:

```rust
// runtime/torrent_service.rs:23-28
let output_folder = request.path.as_ref()
    .filter(|p| !p.trim().is_empty())
    .cloned()
    .or_else(|| Some(default_output_folder.to_string_lossy().into_owned()));
...
overwrite: true,
```

y `read_torrent_source` lee un path local o descarga una URL arbitraria:

```rust
// runtime/torrent_service.rs:1226
if from.starts_with("http://") || from.starts_with("https://") {
    return state.client.get(from).send().await?...;
}
tokio::fs::read(from).await
```

**Por qué importa.** Cualquier página web abierta en la misma máquina puede hacer `fetch("http://127.0.0.1:11470/removeAll")` (GET simple, sin preflight), expulsar torrents activos, escribir los contenidos de un torrent en un directorio arbitrario (`path`), leer archivos locales (`from`) y usar el cliente HTTP del proceso para SSRF contra la LAN o metadatos de cloud. En una red compartida, cualquier dispositivo puede hacer lo mismo contra el puerto 11470 del equipo. El header `x-stremio-playback-owner` es aceptado sin validar el peer, lo que permite además suplantar la sesión de otro dispositivo y provocar `remove_if_unowned`.

**Corrección.**
1. Enlazar a `127.0.0.1` salvo que `multi_user == true` (y en ese caso exigir configuración explícita y firewall).
2. Token bearer por arranque, generado aleatoriamente, exigido en todas las rutas; el shell lo inyecta al core.
3. `Origin` allow-list (`127.0.0.1`, `localhost`, `tauri://...`) o eliminar CORS por completo. Requerir `POST` para mutaciones.
4. Validar `path` contra un root fijo (o eliminarlo del contrato público) y `overwrite: false`.
5. Restringir `from` a `https?` con allow-list de hosts y límite de tamaño; rechazar IPs privadas/loopback/link-local tras resolver DNS.
6. Ignorar `x-stremio-playback-owner` si el peer no es loopback.

**Esfuerzo:** M (1-2 días, incluye tests de integración).

---

### BUG-01 · Crítica — Fuga permanente de `active_streams`: los torrents nunca se limpian

**Archivo:** `stremio-runtime/src/runtime/torrent_http.rs:238`, `:573`, `:667`; `runtime/torrent_service.rs:360-397`

**Qué ocurre.** `stream_started()` se llama inmediatamente después de obtener el handle:

```rust
// torrent_http.rs:238
state.torrents.stream_started(&normalized_info_hash).await;
```

El único `stream_finished()` asociado se ejecuta en el `on_drop` del stream de respuesta:

```rust
// torrent_http.rs:667
let on_drop = move || {
    tokio::spawn(async move { torrents.stream_finished(&hash).await; });
};
```

Pero el body solo se construye al final (`:689`). Entre `:238` y `:689` hay ~15 salidas tempranas: timeout de metadatos (`:266`, `:294`), fallo de `resolve_file_index`/`file_for_handle` (`:324`), `410` de descarga cancelada (`:355`), fallo/timeout de `stream()` (`:453`), fallo/timeout de `seek()` (`:504`), **toda petición HEAD** (`:573`), `body_len == 0`, timeout/EOF de la primera lectura (`:600-650`). Ninguna decrementa.

**Por qué importa.** `cleanup_inactive` descarta cualquier hash con `active > 0`:

```rust
// torrent_service.rs:389-396
let active = self.active_streams.read().await.get(&hash).copied().unwrap_or(0);
if active > 0 { continue; }
```

Un solo `503` o un `HEAD` de un reproductor deja el torrent (y sus datos en disco) vivo hasta cerrar el proceso. MPV/yt-dlp reintentan con frecuencia y Stremio hace probes HEAD, así que el contador crece de forma monótona. El resultado es uso de disco/RAM creciente, DHT y conexiones mantenidas, y una sesión que empeora con el tiempo.

**Corrección.** Hacer el contador RAII. Crear el guard justo después de `stream_started` y moverlo al `body`/respuesta para que se libere en todas las rutas, incluidas HEAD y errores:

```rust
struct StreamGuard { svc: Arc<TorrentService>, hash: String }
impl Drop for StreamGuard {
    fn drop(&mut self) {
        let svc = self.svc.clone();
        let hash = self.hash.clone();
        tokio::spawn(async move { svc.stream_finished(&hash).await; });
    }
}
let _guard = StreamGuard { svc: state.torrents.clone(), hash: normalized_info_hash.clone() };
```

Para respuestas con body, mover el guard a `WindowTrackingStream` junto al `on_drop`; para HEAD/errores, dejarlo caer al retornar.

**Esfuerzo:** S (2-3 h + test de regresión).

---

### BUG-02 · Crítica — Una única conexión Turso compartida sin mutex entre todas las tareas

**Archivo:** `core-env/src/lib.rs:498-534`

**Qué ocurre.** `get_db_conn()` cachea una `turso::Connection` y devuelve un clon:

```rust
if let Some(conn) = read_database_slot(&DB_CONNECTION) {
    return Ok(conn);
}
...
*DB_CONNECTION.write()... = Some(conn.clone());
Ok(conn)
```

En Turso 0.7.2, `Connection::clone` **no abre una conexión nueva**: comparte el mismo `Arc<TursoConnection>` y conserva un `dangling_tx: AtomicDropBehavior`. Es decir, todas las tareas comparten el mismo handle de protocolo, sin `Mutex`, mientras `app/src/db.rs` sí envuelve su conexión en `AsyncMutex`.

**Por qué importa.**
- `conn.transaction()` (`lib.rs:445`) cambia el estado de transacción **de toda la conexión**. Otra tarea que ejecute un `execute` sobre su “clon” entra silenciosamente en esa transacción y se confirma o revierte con ella.
- El mecanismo `dangling_tx` de Turso puede emitir `ROLLBACK`/`COMMIT` en la siguiente sentencia, revirtiendo trabajo ajeno.
- Un cursor `Rows` puede avanzarse mientras otra tarea prepara sentencias en el mismo handle.
- Se saltan el mutex global de `db.rs`: dos conexiones distintas al mismo archivo con `busy_timeout = 1000` y errores ignorados.

**Corrección.** Devolver un guard de un `AsyncMutex<Connection>` compartido (patrón `MappedMutexGuard` de `db.rs`), o abrir una conexión nueva por operación (`db.connect()` ya se usa y es barata). Nunca compartir un `Connection` entre tareas independientes. Añadir `PRAGMA foreign_keys = ON` a la conexión de `core-env` (hoy solo está en la de `db.rs`).

**Esfuerzo:** M (medio día + revisión de todas las llamadas).

---

### BUG-03 · Crítica — Un JSON de configuración corrupto borra toda la configuración del usuario

**Archivo:** `app/src/config.rs:241-284`

**Qué ocurre.** `init_config()` encadena `if let Ok(...)`. Si falla la lectura de la BD **o** la deserialización, `loaded_from_db` queda en `false` y se escribe inmediatamente el default:

```rust
if !loaded_from_db
    && let Ok(conn) = crate::db::get_conn().await
    && let Ok(serialized) = serde_json::to_string(&config)
{
    let _ = conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('app_config', ?)",
        [serialized],
    ).await;
}
```

**Por qué importa.** `AppConfig` es grande y evoluciona. Cualquier campo renombrado, `null` inesperado, valor de una versión más nueva, escritura truncada o **error transitorio de BD** (`Busy`, `DB not initialized`) hace que el arranque persista los defaults encima de la configuración real. Se pierden tema, geometría, orden de filas, PiP, región, gradientes, etc., sin backup ni aviso. Es pérdida de datos silenciosa e irreversible.

**Corrección.** Distinguir tres estados: fila ausente (escribir default), fila ilegible (no sobrescribir; conservar el valor crudo en `settings/app_config.corrupt-<ts>`, loguear y arrancar con defaults **en memoria**), fila válida (usar). Añadir un test que introduzca `{"broken":` y verifique que la fila original sigue intacta.

**Esfuerzo:** S (2-4 h).

---

### BUG-04 · Crítica — Destino IPv6 en `/proxy` provoca panic remoto

**Archivo:** `stremio-runtime/src/runtime/proxy.rs:24-28`, `:59-69`

**Qué ocurre.**

```rust
let destination_url = url::Url::parse(&destination).context("parsing d")?;
let destination_origin = url_origin(&destination_url)?;
let destination_base = url::Url::parse(&destination_origin)
    .expect("origin URL should always parse")
```

`url_origin()` construye el origen con `format!("{}://{}", scheme, host_str())`. Para un literal IPv6, `host_str()` devuelve `::1` **sin corchetes**, produciendo `http://::1:8080`, que `Url::parse` rechaza. El `expect` entra en panic dentro de la tarea de la petición. No hay `CatchPanicLayer`.

**Por qué importa.** Cualquier cliente local (o una web, dado SEC-01) puede enviar `/proxy/d=http%3A%2F%2F%5B%3A%3A1%5D%3A8080/x` y provocar el panic. En Axum, un panic en una tarea de conexión puede abortar esa conexión; con `panic = "abort"` en release, si el panic ocurre fuera del runtime de Axum o el perfil lo propaga, tumba el proceso entero. Además, `proxy_opts_encode_nested` reutiliza el origen mal formado, así que los segmentos HLS/DASH de un CDN IPv6 también fallan.

**Corrección.**

```rust
fn url_origin(url: &url::Url) -> anyhow::Result<String> {
    let host = url.host_str().context("URL missing host")?;
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    // ...
}
```

Mejor aún, usar `url.origin().ascii_serialization()` y propagar el error con `?` en vez de `expect`. Añadir `CatchPanicLayer` y un test con `http://[::1]:8080/`.

**Esfuerzo:** S (1-2 h).

---

### BUG-05 · Crítica — Un `expect` en el bucle de proyección congela la UI para siempre

**Archivo:** `app/src/event_loop.rs:402` (y `:166`)

**Qué ocurre.**

```rust
// event_loop.rs:402
let model = runtime.model().expect("model read failed");
```

Este es el único camino que empuja `NewState` a Slint. Si la lectura falla (lock envenenado por un panic ajeno, error transitorio del runtime), la tarea larga hace panic y termina. La UI sigue viva, pero **nunca recibe otra actualización**: parece colgada y no se recupera sin reiniciar. Lo mismo ocurre en `schedule_debrid_annotations` (`:166`) y en cinco callbacks de `models/discover.rs` (`:80`, `:115`, `:149`, `:265`, `:377`).

**Por qué importa.** Un `panic` en cualquier hilo que toque el `RwLock` del modelo envenena el lock. A partir de ahí, cada actualización de estado mata el proyector. Es un fallo en cascada difícil de diagnosticar porque el log solo muestra un panic y la UI queda “viva pero muerta”.

**Corrección.** Tratar el error como recuperable: `let Ok(model) = runtime.model() else { tracing::warn!(...); continue; };` y mantener el bucle. Para los callbacks cortos, devolver silenciosamente y reintentar en el próximo evento. Nunca usar `expect` en tareas de larga vida.

**Esfuerzo:** S (1 h).

---

## 4. Hallazgos altos

### SEC-02 · Alta — SSRF y buffering ilimitado en subtítulos, OpenSubtitles, proxy y `/create`

**Archivo:** `runtime/torrent_service.rs:1226-1245`, `runtime/subtitles.rs:74,105-133`, `runtime/opensub.rs:209`, `runtime/proxy.rs:472-485`

Todas estas rutas aceptan una URL del llamante y hacen `.bytes().await` sin límite de tamaño, con el cliente HTTP que sigue redirects. Un servidor que responda 1 Gbit/s durante los 90 s de timeout puede reservar ~11 GB. `opensub.rs` comprueba el tamaño **después** de bufferear. `subtitles_proxy` copia el cuerpo dos veces (`from_utf8_lossy` + `bytes.clone()`). Combinado con SEC-01, es un OOM remoto trivial.

**Corrección:** validar esquema y resolver DNS contra rangos privados; deshabilitar redirects o revalidar cada hop; usar `.bytes_stream()` con tope (`8 MB` subtítulos, `10 MB` torrent, `128 KB` OpenSub) y abortar al superarlo; no copiar el cuerpo dos veces.

**Esfuerzo:** M.

---

### SEC-03 · Alta — El actualizador ejecuta un instalador sin verificación criptográfica

**Archivo:** `app/src/updater.rs:407-456`

`self_update` descarga el asset a `%LOCALAPPDATA%\stremio-native\updates\<version>\stremio-installer.exe` y luego `Command::new(installer).spawn()`. No hay comprobación de checksum, firma Authenticode ni minisign en ningún punto (`grep verify|sha256|signature` no devuelve nada en el archivo). El directorio es escribible por el usuario y el lapso entre staging y `launch_pending()` abarca todo el apagado.

**Por qué importa:** cualquier proceso del mismo usuario, o un archivo dejado en esa ruta, puede sustituir el binario y obtener ejecución de código con los privilegios del usuario. La confianza descansa solo en TLS y en que nadie toque el archivo después.

**Corrección:** publicar y verificar una firma detached (ed25519/minisign) o `WinVerifyTrust` inmediatamente antes de `spawn`; comparar SHA-256 con el release metadata; crear el directorio con ACL de propietario; no aceptar `updated() == false` si el archivo ya existía.

**Esfuerzo:** M.

---

### SEC-04 · Alta — Un keyring bloqueado impide arrancar la aplicación

**Archivo:** `app/src/secure_settings.rs:13-41`; `app/src/main.rs:506-508`

`activate_profile` propaga `Missing`/`Locked` del keyring, y `finish_startup` lo usa con `?`. Una integración cosmética (TheIntroDB) bloquea el arranque completo si el secreto no está, si la máquina es nueva, si se restauró el perfil o si Linux no tiene Secret Service.

**Corrección:** tratar `Missing`/`Locked` como “sin clave”: cachear vacío, loguear warning, poner `enabled = 0` y continuar. Solo abortar si el vault es realmente necesario para una función imprescindible.

**Esfuerzo:** S.

---

### BUG-06 · Alta — Rangos insatisfacibles devuelven el archivo completo; un `idx` fuera de rango sirve otro archivo

**Archivo:** `runtime/torrent_service.rs:1374-1398`, `:1101-1118`; `runtime/torrent_http.rs:393-401`

`parse_range` devuelve `None` tanto para “sin Range” como para `bytes=999999999-` o `bytes=5-3`. El llamante interpreta `None` como ausencia de Range y responde **200 OK con el archivo entero**, en lugar de `416 Range Not Satisfiable`. Un reproductor que prueba la cola (`bytes=-1`) recibe gigabytes. Además, `resolve_file_index` acepta cualquier entero no negativo; si está fuera de rango, cae en `guess_index`, que devuelve el archivo de vídeo más grande: el cliente recibe **contenido equivocado con 200**.

**Corrección:** introducir `RangeParse::Unsatisfiable` → `416` + `Content-Range: bytes */len`; si el `idx` numérico parsea pero `>= files.len()`, devolver `404`; usar `guess_index` solo cuando el segmento no sea numérico y haya filtros `f=`.

**Esfuerzo:** S-M.

---

### BUG-07 · Alta — `RwLock` retenido durante llamadas de red en `local_addon`

**Archivo:** `runtime/local_addon.rs:294-298`, `:372-382`

```rust
let entries = state.local_addon.entries.read().await;
let Some(entry) = entries.values().find(|entry| entry.item_id == id).cloned() else { ... };
let mut meta = map_local_addon_entry_to_meta(&state.client, &entry, media_type).await
```

El guard de `entries` no se libera antes del `await` (no hay `drop(entries)`), así que se mantiene durante una llamada a Cinemeta de hasta 6 s. Peor: el guard de `handles` (`:372`) se mantiene durante **una llamada por archivo de cada torrent**. `tokio::sync::RwLock` es write-preferente: en cuanto un `add_magnet`/`remove` espera `write()`, todos los `read()` posteriores (incluido cada `TorrentService::get()` del streaming) se encolan detrás.

**Corrección:** clonar el `Arc`/entrada y hacer `drop(guard)` explícito antes del `await`; cachear Cinemeta por `(nombre, año)`; limitar la concurrencia con `buffer_unordered(8)`.

**Esfuerzo:** S-M.

---

### BUG-08 · Alta — Carreras y colisiones en el gestor de descargas

**Archivo:** `runtime/downloader.rs:226-252`, `:291-292`, `:420-432`

- El task se lanza antes de insertar el `JoinHandle`; si termina rápido (404, archivo pequeño), su `remove` no encuentra nada y el handle queda en `tasks` para siempre.
- `download_path()` es `dir.join(nombre_sanitizado)` sin id: dos descargas con basename `video.mp4` escriben el mismo archivo, y `remove()` de una borra el de la otra.
- `persist()` clona todo el mapa y reescribe el JSON con `tokio::fs::write` desde cada tarea cada 2 s, sin mutex: dos persists pueden intercalarse y dejar un JSON truncado. Al arrancar, `from_slice(...).unwrap_or_default()` descarta silenciosamente **todos** los registros.
- `resume()` puede lanzar dos `run_download` para el mismo id.

**Corrección:** insertar un placeholder bajo el lock antes de `spawn`; nombrar `format!("{id}-{nombre}")` o usar subdirectorio por id; serializar `persist` con un mutex dedicado y escribir a `.tmp` + `rename`; throttle global; guardar backup del JSON corrupto en vez de default.

**Esfuerzo:** M.

---

### BUG-09 · Alta — Bomba de descompresión en imágenes y `cover_art_file` sin semáforo

**Archivo:** `app/src/image_cache.rs:477-553`, `:586-618`

Solo se limita el cuerpo comprimido (`MAX_RESPONSE_BYTES = 16 MiB`); la decodificación confía en las dimensiones de la cabecera. `ImageReader::into_decoder()` usa los `Limits` por defecto de `image` 0.25: `max_alloc` no estricto de 512 MiB y sin límite de ancho/alto. Un PNG de 16 MiB puede declarar 20000×20000 y reservar cientos de MiB antes de redimensionar. Con `MAX_CONCURRENT_DECODES = 6`, son varios GiB potenciales. `cover_art_file` llama `decode_image` en un `spawn_blocking` sin pasar por el semáforo y reencoda a PNG, multiplicando el pico.

**Corrección:** construir el decoder con `Limits { max_image_width: Some(2048), max_image_height: Some(2048), max_alloc: Some(64 << 20) }` y rechazar antes de asignar; hacer que `cover_art_file` adquiera el semáforo; añadir un presupuesto de bytes en vuelo.

**Esfuerzo:** S.

---

### BUG-10 · Alta — Fallo de inicialización de BD se ignora y el backup no es consistente

**Archivo:** `app/src/db.rs:138-170`; `app/src/main.rs:184-186`

`create_pre_migration_backup` copia el `.db`, `-wal` y `-shm` en tres operaciones independientes, sin lock, checkpoint ni snapshot. El resultado puede ser una imagen SQLite inválida. Además usa `fs::copy` síncrono dentro de un `async fn` llamado desde el runtime, y cualquier error se propaga; `main.rs` solo loguea un warning y continúa. A partir de ahí `get_conn()` devuelve `"DB not initialized"` y todas las escrituras (config, historial, biblioteca, descargas) se pierden en silencio.

**Corrección:** hacer el backup en `spawn_blocking` con `PRAGMA wal_checkpoint(TRUNCATE)` + `VACUUM INTO`, o copiar tras un cierre limpio; tratar el fallo de init como fatal con diálogo de recuperación; podar backups antiguos.

**Esfuerzo:** M.

---

### BUG-11 · Alta — Un único mutex serializa toda la BD y el apagado puede abandonar el checkpoint

**Archivo:** `app/src/db.rs:15-16`, `:546-584`; `app/src/main.rs:348-356`

Todas las lecturas/escrituras de la app pasan por `OnceLock<AsyncMutex<Option<Connection>>>`. Una consulta lenta o un `Busy` con reintento de 1 s bloquea ajustes, descargas y proyecciones. En el apagado, `slot.lock().await` no tiene timeout; los llamantes envuelven todo en 2 s, así que si hay una consulta en vuelo el timeout salta, la conexión no se toma, `core_env::shutdown_database()` no se ejecuta, el WAL no se checkpointa y el instalador del updater arranca sobre una BD aún abierta.

**Corrección:** usar `try_lock`/timeout en el take de apagado; marcar un flag `shutting_down` que `get_conn` rechace; añadir un pool de lecturas (`Database::connect()` por lector) en vez de un mutex global; checkpoint en `spawn_blocking`.

**Esfuerzo:** M-L.

---

### BUG-12 · Alta — `flush_storage` drena antes de comprobar la BD y pierde escrituras

**Archivo:** `core-env/src/lib.rs:537-558`

```rust
let pending_items = { let mut pending = pending_storage().lock()...; pending.drain().collect() };
if pending_items.is_empty() { return; }
let Ok(conn) = get_db_conn().await else { return; };
```

Si `get_db_conn()` falla (apagado, BD no inicializada, `Busy`), los datos de biblioteca/progreso ya se han eliminado del mapa en memoria y nunca llegan a disco. Cada `execute` ignora su error y no hay transacción: un fallo en la fila 1 descarta las demás.

**Corrección:** obtener la conexión primero; en fallo, reinsertar en `pending_storage`; ejecutar todo en una transacción y propagar/loguear errores; reintentar una vez en `Busy`.

**Esfuerzo:** S.

---

### BUG-13 · Alta — Cualquier evento del sistema de archivos dispara un rescan completo y se pierden eventos

**Archivo:** `app/src/local_library.rs:177-187`, `:537-555`

El watcher hace `sleep(750ms)`, drena el canal y llama `manager.scan(root)`, que recorre todo el árbol con `WalkDir` y ejecuta `inspect_media_file` (metadata, BLAKE3, NFO, subtítulos) por cada vídeo. Si la raíz incluye la carpeta de descargas, cada chunk escrito genera eventos y un rescan. Además el productor usa `try_send` a un canal de 64: los eventos que llegan durante el sleep/scan se descartan, así que cambios reales pueden perderse.

**Corrección:** filtrar a extensiones de vídeo, debounce con temporizador (no `sleep` en el consumidor), mantener un set de rutas sucias y hacer actualizaciones incrementales por archivo; usar `send().await` o un canal que fusione eventos.

**Esfuerzo:** M.

---

### BUG-14 · Alta — `loadfile` síncrono bloquea el actor MPV y desborda la cola

**Archivo:** `playback-mpv/src/actor.rs:495-501`, `:1301-1322`, `:1672`

`client.command(&["loadfile", ...])` es síncrono. Con `ytdl_hook` la resolución de red ocurre dentro del hilo del actor, que no vuelve a `drain_events` hasta terminar. La cola de comandos es de 128; al llenarse, `PlaybackController::send` devuelve `CommandQueueFull` y los comandos del usuario se pierden. MPV desborda su propia cola de eventos (`EVENT_QUEUE_OVERFLOW`), que solo se loguea como warning, dejando `paused`/`time-pos`/EOF obsoletos. En el peor caso, `shutdown()` usa `send()` bloqueante y `join()` en el hilo que lo llama (UI), congelando la app durante la operación de red.

**Corrección:** usar `command_async` para `loadfile`, `sub-add` y `screenshot-to-file`, correlacionar por `reply_userdata` y seguir drenando eventos; separar el comando de carga en un hilo auxiliar; en teardown, `try_send` + flag atómico y `join` con timeout (o detach), nunca bloqueante desde la UI.

**Esfuerzo:** L.

---

### BUG-15 · Alta — El callback `wakeup` solo se desregistra en la salida feliz → UAF tras panic

**Archivo:** `playback-mpv/src/actor.rs:574-577`, `:1244-1247`; `thumbnail.rs:249-252,465`; `preview.rs:254-257,506`

Se pasa a libmpv un puntero crudo a `Arc<ActorWake>`:

```rust
client.set_wakeup_callback(Some(wakeup_actor), Arc::as_ptr(&wake).cast_mut().cast::<c_void>());
```

La desregistración está **después** del bucle `while running`, sin guard. Cualquier panic dentro del actor (por ejemplo, un sink de usuario que entre en panic) desenrolla y salta esa línea. El `Arc<MpvClient>` sigue vivo en `RenderSource`/`RenderContext`; cuando se libera el `ActorWake`, libmpv conserva el puntero y el siguiente wakeup escribe en memoria liberada.

**Corrección:** un guard con `Drop` que ejecute `set_wakeup_callback(None, null)`; envolver el cuerpo del actor en `catch_unwind` y reportar el panic como error. Aplicar el mismo patrón a thumbnail y preview.

**Esfuerzo:** M.

---

### BUG-16 · Alta — Lost wakeup en `wait_for_event`: timeouts espurios de 15 s en miniaturas y previews

**Archivo:** `playback-mpv/src/thumbnail.rs:1036-1044`, `preview.rs:785-793`

`wakeup_thumbnail` incrementa `mpv_wakeup_revision` y llama `notify_one()` **sin tomar el mutex del mailbox**. El worker comprueba la revisión bajo el mutex y luego hace `wait_timeout`. Si el wakeup llega entre la comprobación y el `wait`, la notificación se pierde (Condvar sin waiter la descarta), y el worker duerme hasta el deadline (15/20 s). El evento que la causó (`EVENT_PLAYBACK_RESTART` tras seek, `EVENT_FILE_LOADED`) queda en cola. El resultado es un stall aleatorio de 15 s en el hover de la barra y un `SeekFailed("timed out")` que se reintenta.

**Corrección:** notificar bajo el mutex del mailbox (o eliminar el Condvar y esperar en `mpv_wait_event` con slices cortos de 50 ms comprobando el canal de control). Revisar también `wait_for_activity`.

**Esfuerzo:** S-M.

---

### PERF-01 · Alta — Clonado profundo de submodelos completos en cada `NewState`

**Archivo:** `app/src/event_loop.rs:596-752`

En cada actualización se clonan `continue_watching_preview`, `board`, `addons`, `discover`, `library`, `continue_watching`, `remote_addons`, `addon_details`, `meta_details`, `settings`, `data_export`, `calendar`, `search`, `local_search` y `player`, además de construir un `Vec<(String, String)>` con todos los nombres de addons. Aunque solo se clonan los campos “necesarios”, un tick de progreso o un cambio de ajuste puede copiar cientos de KB/MB y moverlos al closure de UI.

**Por qué importa:** es el camino más caliente de la app y crece con el tamaño de la biblioteca/catálogo. Provoca picos de asignador, presión de GC ausente (Rust) pero sí `malloc`/`mimalloc`, y latencia en el hilo de proyección.

**Corrección estructural:** proyectar a **view-models planos** (structs `Send` con solo los campos que Slint consume) mientras se mantiene el guard de lectura, y aplicar esos view-models en el hilo de UI. Alternativamente, guardar el `AppModel` en un `Arc` y publicar snapshots inmutables por campo. Evitar `model.player.clone()` en cada tick: separar escalares de alta frecuencia (`time`, `buffered`) del estado estructural.

**Esfuerzo:** L.

---

### PERF-02 · Alta — Captura/restauración completa de estado OpenGL en cada frame

**Archivo:** `playback-mpv/src/render.rs:537-598`, `:601-669`, `:1272`, `:1318`

Cada `process_updates` y cada `render` construyen `FrameOpenGlState::capture`, con ~24 `glGetIntegerv/glGetFloatv/glGetBooleanv/glIsEnabled`, y luego `restore` con ~30 setters. Un frame normal que llama a ambos paga la captura dos veces. `glGet*` tras dibujar fuerza sincronización en varios drivers.

**Corrección:** capturar una vez el estado de Slint al crear el contexto y, por frame, restaurar solo lo que MPV documenta que toca (FBO, viewport, programa, VAO, unidad 0, 5 enables). Evitar la segunda captura cuando `process_updates` y `render` van seguidos. Medir con GPU timer/`apitrace`.

**Estado:** corregido. `FrameOpenGlState` se eliminó. `FrameOpenGlStateGuard` ya no consulta el driver: `restore_slint_gl_state` restaura a ciegas el conjunto mínimo (`bind_framebuffer`, `use_program(0)`, `bind_vertex_array(0)`, buffers, `active_texture(TEXTURE0)`, `bind_texture`, `bind_sampler(0)`, `disable` de blend/cull/depth/scissor/stencil, color/depth mask). Cero `glGet*` por frame.

**Esfuerzo:** M.

---

### PERF-03 · Alta — Miniaturas/preview: 3 copias de frame, rotación píxel a píxel y escala CPU

**Archivo:** `playback-mpv/src/thumbnail.rs:1183-1193`, `:1293-1305`, `:950`; `preview.rs:707-720`

Por frame capturado: `parse_screenshot` copia fila a fila a un `Vec`; `rotate_rgba` asigna otro `Vec` y recorre píxel a píxel (incluso para 180°, que es una copia); `Arc::from(rgba)` asigna y copia una tercera vez. A 15 fps y 640×360 son ~2,7 MB/frame de copia extra (~40 MB/s) más el filtro `sws` en CPU. Todo compite con el decodificador principal.

**Corrección:** para 0°/180° usar `copy_from_slice` por filas y `reverse` de filas; para 90/270 procesar por bloques; construir el `Arc<[u8]>` una sola vez; delegar la rotación a MPV (`video-rotate`) y usar `screenshot-raw` sin escalado CPU cuando sea posible.

**Esfuerzo:** M.

---

## 5. Hallazgos medios

> Se agrupan en tabla para mantener el documento accionable. Cada fila cita el archivo y la corrección concreta.

### 5.1 Seguridad y robustez

| ID | Archivo | Problema | Corrección |
|---|---|---|---|
| SEC-05 | `app/src/single_instance.rs:86-135,214` | Canal TCP fijo sin auth, `mpsc::unbounded`, sin timeout de lectura → slowloris y deep links inyectados por cualquier proceso local | Secreto por instalación en archivo `0600`, canal acotado, `timeout(2s)` por conexión, puerto aleatorio en lockfile |
| SEC-06 | `app/src/paths.rs:48-65`, `crash_handler.rs:267-282` | Directorios/archivos con permisos por umask (`0755/0644`): BD, logs y minidumps legibles por otros usuarios | `DirBuilderExt::mode(0o700)` y `OpenOptionsExt::mode(0o600)`; endurecer rutas existentes al arrancar |
| SEC-07 | `stremio-runtime/src/lib.rs:655-668` | `proxy_client` con `danger_accept_invalid_certs(true)` y `danger_accept_invalid_hostnames(true)` → MITM en addons/CDNs | Eliminar salvo opt-in explícito; usar `rustls` con roots del sistema |
| SEC-08 | `core-env/src/http_cache.rs:319-335` | Se reenvían **todas** las cabeceras entrantes (incluidas `Host`, `Content-Length`, `Connection`, auth propietaria) | Allow-list explícita; eliminar hop-by-hop y credenciales |

### 5.2 Correctitud

| ID | Archivo | Problema | Corrección |
|---|---|---|---|
| BUG-17 | `runtime/torrent_service.rs:951` | `upload_speed: download_speed.max(upload_speed)` — la UI muestra subida = bajada mientras descarga; contamina las stats por peer | `upload_speed,` |
| BUG-18 | `runtime/torrent_service.rs:208-224` | `remove()` borra los mapas **antes** de `session.delete`; si falla, el torrent sigue vivo pero invisible | Borrar primero en librqbit; en error, reinsertar el handle o forzar delete verificado con `session.list()` |
| BUG-19 | `runtime/torrent_service.rs:1212-1223` | `parse_json_body` convierte cualquier JSON inválido en `{}` y responde 200 | `400` con el error de serde; solo default si `body.is_empty()` |
| BUG-20 | `app/src/models/details.rs:25-29` | Temporada/episodio/búsqueda/pending en `OnceLock` global: se filtra entre perfiles y títulos | Mover a `AppModel` o a un mapa por `profile_id`; limpiar al cambiar de perfil/ruta |
| BUG-21 | `app/src/mpv_integration.rs:1548-1554` | `ui.upgrade()` desde el task Tokio devuelve `None` (Slint exige el hilo creador): el título y otras escrituras directas se omiten en silencio | Mover toda lectura/escritura de propiedades a `slint::invoke_from_event_loop`; usar atomics para flags del watchdog |
| BUG-22 | `app/src/db.rs:172-215` | Un binario antiguo contra un esquema más nuevo no falla: el rango de migraciones queda vacío y sigue operando | `bail!` si `current_version > LATEST_SCHEMA_VERSION` con diálogo “actualiza la app” |
| BUG-23 | `app/src/updater.rs:345-366` | Cualquier fallo de red/HTTP se mapea a “sin actualizaciones” | Distinguir transporte de “no hay release”; mostrar `Failed` en chequeo manual |
| BUG-24 | `runtime/subtitles.rs:252,274` | Overflow de `i64` con timestamps maliciosos; `&amp;` aplicado a SRT (no es HTML) | `checked_mul/checked_add` + límites (`hours<1000`); escapar solo en VTT y también `-->` |
| BUG-25 | `app/src/config.rs:345-361` | `&hex[0..2]` hace panic si el string tiene 6 bytes pero un carácter multibyte | Validar `is_ascii_hexdigit` o usar `as_bytes().chunks_exact(2)` |
| BUG-26 | `playback-mpv/src/render.rs:547-553,629-641` | `FrameOpenGlState` guarda solo la unidad de textura 0 pero limpia 1..3; no guarda `PIXEL_UNPACK_BUFFER`/`PACK_ALIGNMENT` → estado de Slint se filtra entre frames | **Corregido:** se eliminó `FrameOpenGlState`; `restore_slint_gl_state` fija el baseline mínimo conocido de Slint/FemtoVG sin capturar ni consultar estado |

### 5.3 Rendimiento

| ID | Archivo | Problema | Corrección / impacto |
|---|---|---|---|
| PERF-04 | `playback-mpv/src/preview.rs:462-483` | Si la captura tarda más que `frame_interval`, `next_frame_due` se fija a `now + interval` y el worker captura sin dormir → 100 % de un core | Avanzar desde el deadline anterior (`+= interval`) y resincronizar si va muy atrás |
| PERF-05 | `app/src/models/library.rs:423` | `to_lowercase()` por ítem en cada sync/tecla | Cachear el nombre en minúsculas o usar matcher sin asignación; debounce de búsqueda |
| PERF-06 | `app/src/models/addons.rs:66,545`; `community_addons.rs:562-575` | Clonar todos los `Descriptor` bajo mutex; rebuild completo por tecla; paginación O(n²) leyendo `row_data` de todo el modelo | `Arc<[Descriptor]>`, debounce, `Rc<VecModel>` persistente con `push` incremental |
| PERF-07 | `app/src/navigation.rs:203-205,219-272` | `snapshot()` clona `Vec<Route>` en rutas calientes; `dispatch` clona dos veces y loguea rutas completas a INFO | Snapshot por `Arc`/revisión; log a `debug!` solo si cambió |
| PERF-08 | `app/src/models/discover.rs:80-624` | Un `tokio::spawn` por cambio de filtro sin guard de revisión; reconstrucción de todos los `VecModel` de filtros con un fingerprint que incluye datos de ítems | Debounce + revisión monotónica; fingerprint por sección; actualizar en sitio |
| PERF-09 | `app/src/downloads.rs:681-690,825-845` | `UPDATE` a SQLite cada 300 ms por descarga, antes de leer el siguiente chunk | UI por `watch` cada 300 ms; persistir cada 3-5 s y en transiciones |
| PERF-10 | `app/src/models/settings.rs:1118-1239`; `config.rs:289-327` | `save_config` síncrono en callbacks de UI; `load_config()` clona ~60 strings por llamada; escrituras no ordenadas | Escritor único debounced con `watch`; usar `with_config`; eliminar `tidb_api_key` del struct serializado |
| PERF-11 | `runtime/torrent_http.rs:11-22`; `torrent_service.rs:353-358` | `/stats.json` mantiene el lock de `handles` mientras construye stats de todos los torrents y hace `touch()` de cada uno (impide el cleanup multi-usuario) | Snapshot `Vec<(hash, Arc)>`, `drop(guard)`, construir en paralelo; no `touch` en stats |
| PERF-12 | `runtime/torrent_service.rs:558-642`; `runtime/app_routes.rs:80-86` | Reaper recorre todo el árbol de caché cada 60 s **y** en cada `POST /settings`; `cacheRoot` no se aplica | Índice incremental o contador de bytes; debounce; recrear sesión o rechazar cambio de root |
| PERF-13 | `core-env/src/http_cache.rs:479-496,537-600` | Un `UPDATE` por hit de caché; cada 64 escrituras un `DELETE` + `SUM` full scan + cargar todas las filas para evacuar | LRU en memoria delante de la BD; batch de `accessed_at`; `DELETE ... WHERE cache_key IN (SELECT ... LIMIT ?)`; suma incremental |
| PERF-14 | `runtime/local_addon.rs:536`; `torrent_service.rs:1169,1357` | Regex compilada por cada meta/archivo; query parseada 5 veces y clonada | `OnceLock<Regex>`; compilar `f=` una vez por petición; parsear la query una vez |
| PERF-15 | `server-connector/src/lib.rs:161,350` | `reqwest::Client::new()` sin timeouts; `export_diagnostics` bufferea y copia dos veces; feature `in-process` siempre devuelve `Err` | `.connect_timeout(2s).timeout(30s)`; devolver `Bytes`; implementar o eliminar la feature |
| PERF-16 | `playback-mpv/src/actor.rs:1649-1684` | Se clona `PlaybackState` completo y se asigna un `Arc` nuevo por cada lote de propiedades, incluido `time-pos` (hasta la tasa de frame) | **Parcial:** el actor solo emite si el snapshot cambió; pendiente separar `time-pos`/buffer a un `watch` o `AtomicU64` dedicado |
| PERF-17 | `playback-mpv/src/render.rs:1248-1253` | Al redimensionar se borran texturas/FBO inmediatamente, aunque Slint pueda estar muestreando el id del último frame; se reasigna en cada paso del drag | Ranura “retired” de un frame o `glTexImage2D` sobre los mismos ids; debounce del resize |
| PERF-18 | `app/src/models/details.rs:1349-1351` | La lista de episodios se reemplaza por un `VecModel` nuevo cuando cambia el fingerprint, perdiendo identidad de fila/scroll | Aplicar `set_row_data`/inserciones sobre el modelo existente |

### 5.4 Memoria / almacenamiento

| ID | Archivo | Problema | Corrección |
|---|---|---|---|
| MEM-01 | `app/src/image_cache.rs:665-718` | `DISK_TTL` y `DISK_CAPACITY_BYTES` se aplican **una sola vez** por proceso; sesiones largas crecen sin límite | Mantenimiento periódico (1 h) o al superar ~90 % de capacidad; guardar validadores ETag/Last-Modified |
| MEM-02 | `app/src/image_cache.rs:627-645` | Temp file `tmp-<pid>` compartido entre writers concurrentes del mismo destino | Nombre único (`Uuid`/contador atómico), `sync_all` y rename; verificar tamaño en el caso “ya existe” |
| MEM-03 | `app/src/image_cache.rs:477-553` | Cache de memoria 48 MB + TTL 5 min es correcto, pero el pico de decodificación no está acotado (ver BUG-09) | Presupuesto de bytes en vuelo + límites de decoder |

### 5.5 Bajos (robustez y mantenibilidad)

| ID | Archivo | Problema | Corrección |
|---|---|---|---|
| LOW-01 | `app/src/downloads.rs:899-921` | Ante colisión, prueba hasta 10.000 sufijos con `try_exists` secuencial; TOCTOU entre comprobar y crear | Reservar con `OpenOptions::create_new` en un bucle corto; fallback a UUID |
| LOW-02 | `playback-mpv/src/actor.rs:1655` | `Vec::with_capacity(msg.num_args as usize)` con `num_args` negativo → capacidad gigante y aborto | `usize::try_from(...).unwrap_or(0).min(64)` |
| LOW-03 | `playback-mpv/src/actor.rs:473-476` | El `%len%` de `ytdl_hook-ytdl_path` depende del orden de evaluación; un refactor puede corromper el valor | Construir el payload primero y usar su longitud |
| LOW-04 | `playback-mpv/build.rs:247-249,339-346` | Recalcula SHA-256 de artefactos de ~50 MB en cada build; un fallo de `io::copy` deja `.tmp` huérfanos | Gate por `(mtime, size)` y limpieza con guard `Drop` |
| LOW-05 | `stremio-runtime/src/lib.rs:507-526` | `block_in_place` hace panic en runtime current-thread; si no hay runtime, el `JoinHandle` se descarta y `join()` devuelve `Ok` sin unir nada | Exponer `async fn join()`; usar `spawn_blocking`/`Handle::block_on`; devolver error si no se unió |
| LOW-06 | `stremio-runtime/src/lib.rs:738-749` | Un segundo `start(init_logging=true)` hace panic en `set_global_default` y el guard ignorado rompe el writer existente | `LOG_GUARD.set(...).is_ok()` + `try_init()` |
| LOW-07 | `ARCHITECTURE.md` §5 | Describe overlay Wayland/DirectComposition y “zero-copy” que no existe en el código (todo es FBO); pragmas documentados no coinciden | Actualizar el documento o marcar como trabajo futuro (ver §12) |

---

## 6. Rendimiento: análisis por subsistema

### 6.1 Arranque (cold start)

**Estado actual:** ya se aplicaron las mejoras de `RamStremio.md` y parte de `Roadmap123123.md`: debounce de 60 s para `library`/`library_recent`, `WHERE value != excluded.value`, `mmap_size`, telemetría opcional, barrera de readiness y early auth.

**Riesgos residuales:**

1. **`init_config` + `load_early_auth_state` comparten el mutex global de BD** con la inicialización de Turso. Si el archivo es grande, la ventana antes de `ui.show()` crece. Medir `t_db_init` y `t_early_auth` con `STREMIO_STARTUP_TELEMETRY=1`.
2. **`create_pre_migration_backup`** copia la BD de forma síncrona en el runtime antes de abrirla. En un primer arranque con una BD de cientos de MB, esto es un stall de disco en el hilo async. Mover a `spawn_blocking` y mostrar progreso.
3. **`UiNext.md`/`Roadmap` Fase 2** propone batch de 9 consultas en una sola; hoy `load_startup_storage` sigue haciendo llamadas separadas. Es la mejora de arranque con mejor relación esfuerzo/impacto.
4. **Carga de fuentes/iconos**: `build.rs` embebe fuentes; verificar que no se registran dos veces en runtime (`window_style::register_embedded_fonts`).

**Recomendación:** añadir un test/benchmark de arranque en CI que falle si `t_window_shown > 250 ms` en un runner de referencia, y otro para `t_first_sync`.

### 6.2 Proyección UI (el camino más caliente)

El patrón actual es: `NewState` → lock del modelo → clonar submodelos → `drop(guard)` → `invoke_from_event_loop` → comparar fingerprints → mutar propiedades. Funciona, pero paga dos costes evitables:

- **Clonado profundo** (PERF-01): el coste escala con la biblioteca. Un usuario con 5.000 ítems clona megabytes por tick.
- **Proyección en el hilo de UI**: `models/library.rs`, `discover.rs` y `addons.rs` hacen trabajo O(n) (búsqueda, fingerprints, `VecModel` nuevos) dentro del closure. Ese trabajo debería ocurrir fuera del hilo de UI y llegar como filas ya listas.

**Propuesta concreta (view-models):**

```rust
// Se construye en el hilo de proyección, sin tocar Slint.
struct LibraryRow { id: SharedString, name: SharedString, poster: Option<Url>, progress: f32, watched: bool }

// En el hilo de UI solo se aplica el diff contra el VecModel existente.
fn apply_library(ui: &MainWindow, rows: Vec<LibraryRow>) { /* update_vec_model_with */ }
```

Esto elimina el clonado de `Library`/`Catalog` completos y permite reutilizar el diffing que ya existe (`update_vec_model_with`). Es la optimización de mayor impacto sostenido.

**Otras mejoras de UI:**

- Debounce de búsquedas (biblioteca, addons, discover) a 150-200 ms.
- `navigation.snapshot()` por `Arc` y logs a `debug!` (PERF-07).
- No reconstruir `VecModel` de episodios cuando solo cambia `watched` (PERF-18): usar `set_row_data`.
- Convertir `thumbnail_preview` y `preview_player` a buffers `SharedPixelBuffer` construidos fuera del hilo de UI y limitar la tasa de dispatch a la tasa de refresco.

### 6.3 Reproducción y GPU

**Lo que ya está bien:** el pipeline FBO con doble buffer evita que Slint muestree la textura que MPV está escribiendo; el `FrameCache` de miniaturas está acotado por bytes; no hay `glFinish`/`glReadPixels` en el camino caliente; el `render_update` está protegido con `catch_unwind`.

**Lo que falta:**

1. **Estado GL por frame** (PERF-02): es la mejora más directa en FPS y latencia de UI durante reproducción. Medir con `apitrace` o GPU timer antes/después.
2. **Teardown de texturas** (PERF-17): al redimensionar se borran texturas que Slint puede estar muestreando; usar una ranura “retired” y borrar en el siguiente frame, o reutilizar ids con `glTexImage2D`.
3. **Restauración asimétrica** (BUG-26): `FrameOpenGlState` guarda solo la unidad 0 pero limpia 1..3; guardar todas las unidades o no tocar las que Slint posee. Añadir `PIXEL_UNPACK_BUFFER`/`PACK_ALIGNMENT` al guard.
4. **Actor**: pasar a comandos asíncronos (BUG-14) y emitir `PlaybackState` solo si cambió (`if next == *state { return }`) para evitar un `Arc` y ~10 asignaciones por tick.
5. **Preview**: throttle real (PERF-04), cola acotada (hoy `controls: VecDeque` sin límite), y descartar `Play` obsoletos antes de cargar.
6. **Watchdog de carga** (`mpv_integration.rs:1656-1707`): polling de 500 ms con lecturas de propiedades Slint desde otro hilo. Reemplazar por evento de primer frame + `oneshot`/`Notify`, y limpiar el handle al terminar.

### 6.4 Runtime P2P y HTTP

- **Stats**: `/stats.json` es O(torrents × archivos × peers) bajo lock y hace `touch` (PERF-11). Cachear `source_urls`/`opts` por hash y construir en paralelo.
- **Rangos**: corregir 416 (BUG-06) evita descargas completas accidentales, que son mucho más caras que el propio bug de CPU.
- **Caché de disco**: reaper incremental (PERF-12); hoy recorre miles de archivos cada minuto.
- **Regex/query**: compilar una vez (PERF-14); en un catálogo local grande, `filter_matches_file` se ejecuta por archivo y por petición.
- **Cliente HTTP**: timeouts ya existen en el runtime (15 s connect, 90 s total), pero no hay tope de bytes (SEC-02). Añadir `Content-Length` máximo y streaming con corte.

### 6.5 Persistencia e I/O

- **Debounce**: 60 s para `library` es agresivo para el usuario (hasta 1 min de progreso perdido en un crash). Reducir `library_recent` a 10-15 s y forzar flush en pausa/seek/cierre de player (ya existe `flush_storage`; verificar que se llama en todos esos puntos).
- **Config**: un escritor único debounced evita carreras y jank (PERF-10).
- **Descargas**: separar progreso UI de persistencia (PERF-09).
- **HTTP cache**: LRU en memoria + batch de `accessed_at` (PERF-13). Para catálogos, esto elimina un `UPDATE` por hit y un full scan cada 64 escrituras.
- **Checkpoint**: el apagado con timeout de 2 s puede abandonar el WAL (BUG-11); usar `try_lock` y un checkpoint en `spawn_blocking`.

---

## 7. Seguridad: modelo de amenazas resumido

| Actor | Superficie | Impacto actual | Mitigación |
|---|---|---|---|
| Página web abierta en la misma máquina | `http://127.0.0.1:11470` + CORS `*` | Expulsar torrents, escribir archivos, leer archivos, SSRF | Token + Origin allow-list + POST |
| Dispositivo en la LAN | `0.0.0.0:11470` | Igual que arriba, sin necesidad de navegador | Bind loopback por defecto |
| Proceso local malicioso | Puerto 35984 (single instance) | Inyectar deep links/magnets | Secreto por instalación + socket `0600` |
| Addon malicioso | URLs de pósters/subtítulos/torrents | Decode bomb, SSRF, OOM, path traversal en descargas | Límites de tamaño, validación de esquema/IP, sanitización |
| Atacante de red (MITM) | `proxy_client` con certs inválidos | Interceptar tráfico de addons/CDNs | Quitar `danger_accept_invalid_*` |
| Usuario local sin privilegios | BD, logs, minidumps en `0644` | Leer tokens, historial y memoria del proceso | Permisos `0700/0600` |

**Prioridad absoluta:** SEC-01. Es la única que convierte todos los demás bugs locales en remotamente explotables.

---

## 8. Plan de acción priorizado

### Fase 0 — “No distribuir sin esto” (1-2 días)

1. **SEC-01**: bind loopback + guard de `Origin`/`Sec-Fetch-Site`/`Host` + CORS restringido + `path`/`from` limitados. *(Token por sesión: pendiente de cablear app↔runtime.)*
2. **BUG-01**: guard RAII de `active_streams` con contador atómico por infohash. *(Hecho: sin `tokio::spawn` en `Drop`.)*
3. **BUG-04**: corchetes IPv6 + `?` en vez de `expect`. *(Hecho.)*
4. **BUG-05**: eliminar `expect` del proyector y de los callbacks de discover. *(Hecho.)*
5. **BUG-17**: `upload_speed` (una línea). *(Hecho.)*
6. **BUG-03**: no sobrescribir config si la fila existe pero no parsea. *(Hecho.)*
7. **BUG-25**: `parse_color` sin slicing por bytes. *(Hecho.)*
8. **SEC-04**: el keyring bloqueado/ausente ya no aborta el arranque. *(Hecho.)*
9. **BUG-21**: `ui.upgrade()` solo dentro de `invoke_from_event_loop` en `sync_player`. *(Hecho.)*

**Criterio de salida:** `curl` a `/removeAll` desde otro origen falla con 403/401; un HEAD de stream no deja el torrent vivo; un `/proxy` IPv6 devuelve 400, no panic; la app arranca sin gnome-keyring/kwallet; el título del reproductor se actualiza desde el task Tokio.

### Fase 1 — Integridad de datos y red (3-5 días)

- **BUG-02** (conexión Turso serializada con guard). *(Hecho.)*
- **BUG-10/BUG-11** (init fatal, backup consistente, shutdown con timeout).
- **BUG-12** (flush_storage sin pérdida). *(Hecho.)*
- **BUG-06** (416 + idx fuera de rango). *(Hecho.)*
- **BUG-07** (drop de guards en local_addon). *(Hecho.)*
- **BUG-09** (límites de decoder + semáforo).
- **SEC-02** (topes de bytes y allow-list de esquema).

### Fase 2 — Reproducción y estabilidad (1-2 semanas)

- **BUG-14** (comandos async en el actor). *(Hecho para `loadfile`.)*
- **BUG-15** (guard de desregistro + catch_unwind).
- **BUG-16** (Condvar bajo mutex o espera por slices).
- **BUG-08** (descargas: ids, persist atómico, lock).
- **BUG-13** (watcher incremental).
- **PERF-02/PERF-03/BUG-26** (GL y miniaturas). *(PERF-02 y BUG-26 hechos; PERF-03 pendiente.)*
- **PERF-04** (throttle de preview) y cola acotada.

### Fase 3 — Rendimiento sostenido (2-4 semanas)

- **PERF-01** (view-models planos + proyección fuera del hilo de UI).
- **PERF-05/06/07/08** (búsquedas, addons, navegación, discover).
- **PERF-09/10/13** (persistencia y caché HTTP).
- **PERF-11/12/14** (stats, reaper, regex).
- **MEM-01** (mantenimiento periódico de caché de imágenes).
- Tests de regresión de rendimiento en CI (arranque, proyección, memoria).

---

## 9. Parches sugeridos (copiar/pegar)

### 9.1 Guard RAII para streams (sin `tokio::spawn` en `Drop`)

`active_streams` guarda un contador atómico por infohash. El incremento y la
inserción ocurren bajo el mismo lock, de modo que un `start` concurrente con el
último `drop` no puede resucitar una entrada a cero. `Drop` solo hace un
`fetch_sub` síncrono: es seguro durante el apagado de Tokio y en hilos sin
contexto de runtime.

```rust
// lib.rs
active_streams: std::sync::Mutex<HashMap<String, Arc<AtomicUsize>>>,

// torrent_service.rs
async fn stream_started(&self, info_hash: &str) {
    self.touch(info_hash).await;
    let mut active = self.active_streams.lock().unwrap_or_else(|p| p.into_inner());
    active
        .entry(info_hash.to_string())
        .or_default()
        .fetch_add(1, Ordering::AcqRel);
}

/// Llamado desde `Drop`: nunca hace spawn ni espera.
fn stream_finished(&self, info_hash: &str) {
    let mut active = self.active_streams.lock().unwrap_or_else(|p| p.into_inner());
    let remove = active
        .get(info_hash)
        .is_some_and(|counter| counter.fetch_sub(1, Ordering::AcqRel) <= 1);
    if remove {
        active.remove(info_hash);
    }
}

// torrent_http.rs
struct StreamLease {
    torrents: Arc<TorrentService>,
    info_hash: String,
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.torrents.stream_finished(&self.info_hash);
    }
}

let stream_lease = StreamLease {
    torrents: Arc::clone(&state.torrents),
    info_hash: normalized_info_hash.clone(),
};
// En éxito: mover el lease al body para que viva hasta la desconexión.
// En HEAD/errores: dejarlo caer al salir de la función.
```

### 9.2 Proyector tolerante a fallos

```rust
let model = match runtime.model() {
    Ok(model) => model,
    Err(error) => {
        tracing::warn!(%error, "model read failed; skipping UI projection");
        continue;
    }
};
```

### 9.3 IPv6 en proxy

```rust
fn url_origin(url: &url::Url) -> anyhow::Result<String> {
    let host = url.host_str().context("URL missing host")?;
    let host = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}
```

### 9.4 Límites de decodificación de imágenes

```rust
let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
reader.limits(image::Limits {
    max_image_width: Some(2048),
    max_image_height: Some(2048),
    max_alloc: Some(64 << 20),
    ..Default::default()
});
let decoder = reader.into_decoder()?;
```

### 9.5 Configuración sin clobbering

```rust
match conn.query("SELECT value FROM settings WHERE key = 'app_config'", ()).await {
    Ok(mut rows) => match rows.next().await {
        Ok(Some(row)) => {
            let raw: String = row.get(0)?;
            match serde_json::from_str::<AppConfig>(&raw) {
                Ok(parsed) => { config = parsed; loaded_from_db = true; }
                Err(error) => {
                    tracing::error!(%error, "app_config is corrupt; preserving original row");
                    backup_corrupt_config(&raw).await?;
                    // No escribir defaults encima.
                    loaded_from_db = true;
                }
            }
        }
        Ok(None) => { /* fila ausente: escribir defaults */ }
        Err(error) => { tracing::warn!(%error, "could not read app_config; keeping in-memory defaults"); }
    },
    Err(error) => tracing::warn!(%error, "settings table unavailable"),
}
```

### 9.6 416 correcto

```rust
enum RangeParse { None, Satisfiable(u64, u64), Unsatisfiable }

match parse_range(header, len) {
    RangeParse::None => (StatusCode::OK, 0, len.saturating_sub(1)),
    RangeParse::Satisfiable(start, end) => (StatusCode::PARTIAL_CONTENT, start, end),
    RangeParse::Unsatisfiable => {
        return Ok((StatusCode::RANGE_NOT_SATISFIABLE,
            [("Content-Range", format!("bytes */{len}"))], "").into_response());
    }
}
```

### 9.7 Persistencia de descargas atómica

```rust
let _persist_guard = self.persist_lock.lock().await;
let bytes = serde_json::to_vec(&*self.downloads.read().await)?;
let tmp = self.stats_file.with_extension("json.tmp");
tokio::fs::write(&tmp, &bytes).await?;
tokio::fs::rename(&tmp, &self.stats_file).await?;
```

---

## 10. Verificación y pruebas recomendadas

### 10.1 Pruebas de regresión (unitarias/integración)

1. **Stream guard:** petición HEAD + error 503; comprobar `active_streams[hash]` vuelve a 0 y el torrent se elimina tras `cleanup_inactive`.
2. **Rangos:** `bytes=5-3`, `bytes=999999999-`, `bytes=-1`, `bytes=0-1,5-6`; esperar 416/206 correctos y `Content-Range` válido.
3. **Índice fuera de rango:** `/{hash}/9999` → 404, nunca 200 con otro archivo.
4. **Proxy IPv6:** `d=http://[::1]:8080/x` → 400/502, sin panic.
5. **Config corrupta:** insertar `{"broken":` en `settings` y reiniciar; la fila debe conservarse y crearse un backup.
6. **Turso:** dos tareas concurrentes que abren transacción y escriben; no debe haber interleaving ni rollback ajeno.
7. **Descargas:** dos descargas con el mismo basename → dos archivos distintos; matar el proceso durante `persist` → JSON válido al reiniciar.
8. **Imagen:** PNG con cabecera 20000×20000 y cuerpo pequeño → rechazo antes de reservar.
9. **Keyring:** simular `Missing`/`Locked` → la app arranca con la integración desactivada.
10. **Slint:** `sync_player` desde un hilo no-UI no debe escribir propiedades directamente; test que verifique que el título se actualiza vía `invoke_from_event_loop`.

### 10.2 Pruebas manuales / comandos

```bash
# 1) Exposición de la API (debe fallar con 401 tras el fix)
curl -i http://127.0.0.1:11470/removeAll

# 2) Rango insatisfacible (hoy: 200 + archivo completo; esperado: 416)
curl -s -o /dev/null -w '%{http_code} %{size_download}\n' \
  -H 'Range: bytes=999999999-' \
  http://127.0.0.1:11470/<info_hash>/<idx>

# 3) IPv6 en proxy (hoy: 500/panic; esperado: 400)
curl -i 'http://127.0.0.1:11470/proxy/d=http%3A%2F%2F%5B%3A%3A1%5D%3A8080%2Fx'

# 4) Stats: comprobar que upload != download durante una descarga
curl -s http://127.0.0.1:11470/stats.json | jq '.[].downloadSpeed, .[].uploadSpeed'
```

### 10.3 Rendimiento

- **Arranque:** `STREMIO_STARTUP_TELEMETRY=1` y comparar `t_db_init`, `t_window_shown`, `t_first_sync`.
- **Proyección:** `--profile=ui` y revisar `ui_patches`, `ui_patch_micros`, `Model read lock acquisition took too long`.
- **GPU:** `apitrace` o GPU timer alrededor de `render`/`process_updates`; contar llamadas `glGet*` por frame antes/después de PERF-02.
- **Miniaturas:** medir tiempo de `capture_frame` y CPU del worker con `perf top` durante hover.
- **Runtime:** `curl /stats.json` con 10 torrents y medir latencia; `perf record` del proceso durante streaming.
- **Memoria:** `/proc/<pid>/status` (RSS) y `mimalloc` stats tras navegar 200 carátulas.

### 10.4 CI

- `cargo clippy --workspace --all-targets -- -D warnings` (si el árbol lo permite; hoy hay `unwrap` intencionales que habría que anotar).
- `cargo test --workspace`.
- Benchmark de arranque con umbral (250 ms) y de proyección con biblioteca sintética de 5.000 ítems.
- Test de humo de seguridad: el servidor no escucha en `0.0.0.0` salvo `--multi-user`.

---

## 11. Lo que está bien (verificado)

Para que este documento no se lea como si todo estuviera mal:

- **Sin inyección SQL** en `app/src/db.rs`, `http_cache.rs`, `secure_settings.rs` ni `config.rs`: todas las consultas usan placeholders y parámetros ligados. (`backup.rs:238` interpola nombres de tabla/columna; merece una revisión aparte, pero no se encontró entrada de usuario directa.)
- **Rangos validados** en `parse_range` para `start > end` y `start >= len` (el problema es la respuesta, no el parseo).
- **`parse_screenshot`** valida ancho/alto/stride, strides negativos y overflow antes de copiar (`thumbnail.rs:1119-1200`).
- **`FrameCache` de miniaturas** está acotado por bytes y ordenado LRU (`thumbnail.rs:1362-1409`).
- **Caché de imágenes en memoria** acotada a 48 MB con TTL de inactividad de 5 min; disco reducido a 384 MB (aunque el mantenimiento sea único).
- **No hay `glFinish`/`glReadPixels`/`glFlush`** en el camino caliente; el coste GL es el `glGet*` de PERF-02.
- **Nodos MPV** (`MpvOwnedNode`, `mpv_free`) y constantes ABI están bien gestionados.
- **Timeouts de red** en el runtime (15 s connect, 90 s total) y `WAL` + `busy_timeout`.
- **Crash handler en proceso** con `sigaltstack` y minidump bajo demanda.
- **Debounce de escrituras** de biblioteca y `WHERE value != excluded.value` ya implementados (RamStremio.md).
- **Diffing de `VecModel`** por fila ya existe (`update_vec_model_with`) y se usa en streams/providers; falta extenderlo a más modelos.
- **Telemetría de arranque** opt-in y con overhead cero cuando está desactivada.

---

## 12. Deriva de documentación (baja severidad)

`ARCHITECTURE.md` describe un modo “Hardware Overlay Planes” con `wl_subsurface_set_desync` y DirectComposition, y un fallback FBO. En el código **no existe** ninguna ruta de subsuperficie/DMA-BUF/DirectComposition (`grep` sin resultados); todo pasa por FBO + textura GL. También hay diferencias en los pragmas (`mmap_size = 16777216` en el doc vs `30000000` en el código; `cache_size = -4000` global vs `-2000` en `core-env`).

**Impacto:** las expectativas de rendimiento (“0 % CPU/GPU cuando el OSD está oculto”, “zero-copy”) no se corresponden con el pipeline real. Un lector puede optimizar la cosa equivocada.

**Corrección:** actualizar §5 de `ARCHITECTURE.md` para reflejar el renderer FemtoVG + FBO real, o marcar la sección overlay como “trabajo futuro”. Alinear los pragmas documentados con los del código.

**Estado:** corregido. `ARCHITECTURE.md` §5 documenta el FBO con texturas de doble búfer como diseño definitivo y el overlay `wl_subsurface`/DirectComposition como trabajo futuro no implementado. Los pragmas documentados ahora coinciden con el código (`mmap_size = 30000000`, `cache_size = -4000` en `app`, `-2000` en `core-env`).

---

## 13. Anexo: índice de archivos revisados

| Crate | Archivos |
|---|---|
| `app` | `main.rs`, `event_loop.rs`, `app_model.rs`, `callbacks.rs`, `navigation.rs`, `image_cache.rs`, `thumbnail_preview.rs`, `preview_player.rs`, `mpv_integration.rs`, `playback.rs`, `player_features.rs`, `downloads.rs`, `local_library.rs`, `community_addons.rs`, `deep_link.rs`, `shortcuts.rs`, `db.rs`, `config.rs`, `paths.rs`, `secure_settings.rs`, `single_instance.rs`, `lock_handler.rs`, `crash_handler.rs`, `performance.rs`, `server_readiness.rs`, `startup_metrics.rs`, `updater.rs`, `profiles.rs`, `backup.rs`, `models/{mod,details,discover,settings,library,addons,board,auth,onboarding,search,calendar,pagination,events}.rs` |
| `core-env` | `lib.rs`, `http_cache.rs` |
| `playback-mpv` | `lib.rs`, `actor.rs`, `ffi.rs`, `render.rs`, `thumbnail.rs`, `preview.rs`, `pinned.rs`, `spatial_audio.rs`, `build.rs` |
| `stremio-runtime` | `lib.rs`, `main.rs`, `runtime/{app_routes,torrent_service,torrent_http,proxy,downloader,local_addon,opensub,settings,subtitles}.rs` |
| `server-connector` | `lib.rs` |
| `debrid`, `media-integrations`, `stream-ranking`, `credential-store` | revisados a nivel de API y puntos de contacto; sin hallazgos críticos propios |

**Documentos:** `ARCHITECTURE.md`, `RamStremio.md`, `Roadmap123123.md`, `README.md`, `CHANGELOG.md`, `Streams.md`, `UiNext.md`, `Ventanas.md`.

---

## 14. Conclusión

Stremio Native no tiene un problema de arquitectura: tiene un problema de **confianza en los bordes**. Los bordes son el servidor HTTP local, los addons, el sistema de archivos, el keyring, los archivos de configuración y los callbacks de C. Ahí es donde se concentran los 6 críticos y la mayoría de los altos.

Si solo se puede hacer una cosa, que sea **SEC-01** (cerrar y autenticar el servidor local). Si se pueden hacer tres, añadir **BUG-01** (fuga de torrents) y **BUG-03** (pérdida de configuración). Con eso se elimina la mayor parte del riesgo real de la aplicación. El resto es una lista larga pero mecánica de endurecimiento, RAII, límites y desacoplamiento del hilo de UI.

El rendimiento ya está por encima de la media de un cliente Electron; las mejoras restantes (view-models, estado GL, comandos async de MPV, caché HTTP en memoria) son las que separan “funciona bien” de “se siente instantáneo” en bibliotecas grandes y hardware modesto.

---

## 15. Estado tras la revisión del auditor y correcciones aplicadas

Esta sección responde a la revisión técnica del informe y registra qué se
corrigió, qué quedó parcial y qué sigue pendiente. El objetivo es que la
reauditoría pueda comprobar el estado real del árbol sin volver a leer todo el
documento.

### 15.1 Correcciones aplicadas

| ID | Estado | Cambio |
|---|---|---|
| SEC-01 | **Parcial** | Bind forzado a `127.0.0.1` en modo single-user (el standalone `--multi-user` conserva `0.0.0.0`). CORS solo acepta orígenes loopback. Nuevo `local_request_guard` rechaza `Origin` no loopback, `Sec-Fetch-Site` cross-site y `Host` no loopback. `/create` solo acepta `from` http(s) (nunca rutas locales) y `path` debe estar dentro de `cache_dir`. **Pendiente:** token Bearer aleatorio por sesión entre shell y runtime. |
| BUG-01 | **Corregido** | `active_streams` es `Mutex<HashMap<String, Arc<AtomicUsize>>>`. El incremento ocurre bajo el lock de la entrada; `StreamLease::drop` hace un `fetch_sub` síncrono. No hay `tokio::spawn` en `Drop`. |
| BUG-02 | **Corregido** | `core_env::get_db_conn` devuelve `DbConn` con un `OwnedMutexGuard` que serializa todo acceso protocol-level a la conexión Turso compartida. `http_cache::write_response` suelta la conexión antes de `maintain()` para no auto-bloquearse. |
| BUG-03 | **Corregido** | El loader distingue fila ausente, fila corrupta y error de query. Solo siembra defaults si la fila no existe; una fila corrupta se conserva y se copia a `app_config.corrupt`. |
| BUG-04 | **Corregido** | `url_origin` y `host_header_value` ponen corchetes a IPv6; el `expect` se sustituyó por `context`. |
| BUG-05 | **Corregido** | Los dos `runtime.model().expect(...)` del bucle de proyección y de la reconstrucción de streams ahora hacen `warn` y omiten la proyección. |
| BUG-06 | **Corregido** | Rangos insatisfacibles/malformados devuelven `416` con `Content-Range: bytes */len`. Un índice numérico fuera de rango devuelve error en vez de adivinar otro archivo. |
| BUG-07 | **Corregido** | `local_addon_local_meta` suelta el guard antes de la red; `local_addon_stream` clona los handles antes de await. |
| BUG-12 | **Corregido** | `flush_storage` copia los pendientes, conserva los fallidos y solo elimina la entrada si sigue siendo el mismo valor escrito. |
| BUG-14 | **Corregido** | `loadfile` usa `mpv_command_async` con `LOADFILE_COMMAND_REPLY_ID`; un reply con error emite `PlaybackEvent::Error`. |
| BUG-17 | **Corregido** | `upload_speed` ya no se fuerza a `download_speed.max(upload_speed)`. |
| BUG-18 | **Corregido** | `remove()` borra primero en librqbit y solo después limpia los mapas de bookkeeping. |
| BUG-19 | **Corregido** | JSON inválido devuelve error; `{}` solo se usa para body vacío. |
| BUG-21 | **Corregido** | La actualización de título de `sync_player` hace `upgrade()` dentro de `invoke_from_event_loop`. |
| BUG-25 | **Corregido** | `parse_color` valida que todos los bytes sean hex ASCII antes de slicear; dos tests cubren RGB/ARGB y multibyte. |
| SEC-04 | **Corregido** | Keyring bloqueado/ausente/no disponible ya no aborta el arranque: se emite `warn` y se conserva la clave legacy para migrarla después. |
| PERF-02 / BUG-26 | **Corregido** | Se eliminó `FrameOpenGlState`. `restore_slint_gl_state` restaura a ciegas el baseline mínimo de Slint/FemtoVG sin ningún `glGet*` por frame. |
| PERF-16 | **Parcial** | El actor solo emite `PlaybackEvent::State` si el snapshot cambió. **Pendiente:** mover `time-pos`/buffer a un `watch` o `AtomicU64` y actualizar solo dos escalares en Slint. |
| §12 | **Corregido** | `ARCHITECTURE.md` §5 declara el FBO con doble búfer como diseño definitivo y el overlay/subsurface como trabajo futuro; los pragmas documentados coinciden con el código. |

### 15.2 Contexto solicitado por el auditor

1. **¿El runtime corre in-process o como proceso hijo?**
   In-process. `app/src/main.rs` llama a `stremio_runtime::start(server_cfg)` y
   mantiene el `ServerHandle` en un `Arc<tokio::sync::Mutex<...>>`; el runtime
   corre como tarea Tokio en segundo plano dentro del mismo binario. No hay
   `Command::spawn` para el servidor. Por eso un token Bearer por sesión es
   viable: puede generarse en `main`, pasarse en `ServerConfig` y añadirse a las
   URLs que consume la app/MPV, sin IPC entre procesos.
2. **¿`core-env` y `app/src/db.rs` abren el mismo archivo?**
   Sí. `app/src/db.rs::init_db` abre `stremio.db` y hace
   `core_env::install_database(db.clone())`. Ambos lados comparten el mismo
   `Database`, pero cada uno mantiene su propia `Connection`: `app/src/db.rs`
   la protege con su `AsyncMutex`, `core-env` con el nuevo `DbConn`. BUG-02 ya
   evita el interleaving de transacciones en `core-env`; unificar el handle para
   eliminar las dos conexiones que compiten por el WAL sigue pendiente (relacionado
   con BUG-11).
3. **¿Dónde estaba `FrameOpenGlState`?**
   En `playback-mpv/src/render.rs`. Tenía `capture` (~24 `glGet*`) y `restore`
   (~30 setters) y se instanciaba en `process_updates` y `render`. Ya no existe:
   el guard solo llama a `restore_slint_gl_state`, que emite `glBind*`,
   `glUseProgram(0)`, `glActiveTexture(GL_TEXTURE0)` y `glDisable` para el
   conjunto mínimo que asume FemtoVG.

### 15.3 Pendiente de la Fase 0/1

- **SEC-01:** token Bearer por sesión y, si se mantiene `--multi-user`, un modo
  de autenticación explícito para LAN.
- **BUG-08/09/10/11/13/15/16/20/22/23/24:** descargas, límites de decoder,
  init/backup/shutdown de BD, watcher incremental, desregistro del callback
  `wakeup`, lost wakeup, estado global por perfil y resto de correctitud.
- **SEC-02/SEC-03:** topes de bytes/allow-list de esquema y verificación
  criptográfica del actualizador.
- **PERF-01:** proyección a structs planos `Send` fuera del hilo de UI.
- **PERF-16 completo:** canal `watch`/atomics para `time-pos` y buffer.
- **PERF-03 y resto de PERF/MEM:** pendientes según el plan.

### 15.4 Verificación ejecutada

- `cargo check --workspace`: sin errores.
- `cargo test -p stremio-runtime`: 14 tests OK (incluye loopback y confinamiento de la carpeta de descarga).
- `cargo test -p playback-mpv`: 51 tests OK.
- `cargo test -p stremio-native parse_color`: 2 tests OK.

---

*Fin del informe.*
