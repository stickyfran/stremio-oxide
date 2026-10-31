# Roadmap Técnico y Especificación de Arquitectura — Stremio Oxide (Fase 2)
## Estabilidad Nuclear, Resiliencia de Red, Desbloqueo del Hilo Gráfico y Optimización de E/S

Este documento técnico expande y complementa a `Roadmap_Next.md`. Proporciona el diagnóstico exhaustivo a bajo nivel, la trazabilidad exacta en el código fuente, la comparativa punto a punto entre **Cómo Estaba (Estado Previo)** y **Cómo va a Quedar (Estado Objetivo)**, los **Objetivos Técnicos y Métricas**, y las **Restricciones Críticas de Implementación (Guardrails y Edge Cases)** para guiar la implementación de mejoras de rendimiento y fiabilidad en `stremio-native` sin introducir regresiones en producción.

---

## 1. Gestor de Descargas Resiliente y Streaming HTTP Seguro

### 1.1 Objetivos Técnicos y Métricas
- **Eliminación del límite artificial de 30 segundos en descargas**: Garantizar que descargas de archivos de gran volumen (películas 4K/Remux de 10 GB a 80 GB) puedan completarse sin desconexiones prematuras provocadas por la configuración del cliente HTTP.
- **Integridad de datos en reanudación de archivos parciales (`.part`)**: Prevenir la corrupción silenciosa de archivos de medios mediante la validación estricta de coherencia de recursos entre sesiones (`If-Range`, `ETag`, `Last-Modified`, `Content-Range`).
- **Resiliencia ante rotación de enlaces y expiración de tokens CDN/Debrid**: Permitir la renovación transparente del enlace de descarga si una sesión pausada o interrumpida supera la vida útil del token de acceso (HTTP 401/403/410), sin perder los gigabytes ya descargados.
- **Rendimiento de E/S y control de contención de disco**: Implementar escrituras en búfer alineadas con el sistema de archivos (`tokio::io::BufWriter`) y desacoplar la tasa de refresco de progreso en base de datos.

---

### 1.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto A: Timeout total en lugar de timeout de inactividad
En [`app/src/downloads.rs:435-441`](file:///home/kes/Documentos/Programando/stremio-native/app/src/downloads.rs#L435-L441):
```rust
impl DownloadManager {
    pub fn new(credentials: Arc<dyn CredentialStore>, concurrency: usize) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30)) // <-- DEFECTO CRÍTICO
            .user_agent("Stremio-Native/1")
            .build()
            .unwrap_or_default();
```
**Causa Raíz:** En la biblioteca `reqwest`, el método `.timeout(Duration)` fija el tiempo límite **máximo total** de toda la transacción HTTP, medido desde el apretón de manos inicial hasta el consumo del último byte del cuerpo de respuesta. No es un timeout de inactividad de socket (*idle / read timeout*). Cualquier descarga que tarde más de 30 segundos en transferir sus datos es abortada de forma determinista con un error `reqwest::Error` de tipo *timed out*.

#### Defecto B: Reanudación de rango a ciegas sin verificación de `ETag` ni `If-Range`
En [`app/src/downloads.rs:630-651`](file:///home/kes/Documentos/Programando/stremio-native/app/src/downloads.rs#L630-L651):
```rust
let part_path = part_path(&job.destination);
let resume_from = tokio::fs::metadata(&part_path)
    .await
    .map(|metadata| metadata.len())
    .unwrap_or_default();
let mut request = self.client.get(&source.url);
for (name, value) in &source.headers {
    request = request.header(name, value);
}
if resume_from > 0 {
    request = request.header(RANGE, format!("bytes={resume_from}-"));
}
let response = request.send().await.map_err(|_| DownloadError::Network)?;
if resume_from > 0 && response.status() != StatusCode::PARTIAL_CONTENT {
    return Err(DownloadError::RangeUnsupported);
}
```
**Causa Raíz:**
1. Si el servidor remoto regeneró el archivo, codificó una versión distinta, o si el enlace Debrid expiró y redirige a una página HTML de error con código 206 (o un payload de advertencia), el cliente abre el archivo `.part` en modo `append: true` y concatena bytes corruptos al archivo existente.
2. No se persiste el `ETag` ni el `Last-Modified` en el registro de la base de datos `downloads`.
3. No se valida que la cabecera `Content-Range` devuelta (`bytes 10485760-524288000/524288000`) comience efectivamente en el byte exacto solicitado (`resume_from`).

---

### 1.3 Arquitectura Objetivo y Especificación de Implementación

1. **Configuración de cliente HTTP especializada para Streaming de Archivos**:
   - Separar el cliente de API ligero del cliente de transporte de archivos pesados.
   - Eliminar el `.timeout()` global de la instancia de `DownloadManager`.
   - Configurar `.connect_timeout(Duration::from_secs(20))` y activar TCP Keep-Alive (`.tcp_keepalive(Some(Duration::from_secs(15)))`).
   - Controlar la inactividad durante la lectura de chunks mediante `tokio::time::timeout` envolviendo cada llamada a `response.chunk().await` (timeout de inactividad de 45 segundos).
2. **Protocolo Seguro de Reanudación de Rango**:
   - Modificar la tabla `download_jobs` en SQLite para persistir `remote_etag: Option<String>` y `remote_last_modified: Option<String>`.
   - Al emitir la petición con rango, incluir `If-Range: <etag>` (o fecha si no hay ETag).
   - Analizar la respuesta:
     - Si el servidor responde `206 Partial Content`: Comprobar que la cabecera `Content-Range` coincida con `bytes {resume_from}-...`. Si el rango devuelto no coincide, truncar el archivo a 0 o abortar con error descriptivo.
     - Si el servidor responde `200 OK` (el servidor no soporta rangos o el recurso cambió): Truncar el archivo local `.part` a longitud 0 y comenzar la descarga desde el inicio.
     - Si responde `416 Range Not Satisfiable`: Si `resume_from == content_length`, la descarga ya estaba completa; finalizar con éxito y renombrar de `.part` a destino final.
3. **Escritura Eficiente en Disco**:
   - Envolver el descriptor de archivo de Tokio en `tokio::io::BufWriter::with_capacity(512 * 1024, file)` (búfer en RAM de 512 KB) para no someter al subsistema de archivos a miles de micro-escrituras directas por segundo en conexiones de alta velocidad (1 Gbps+).
   - Agrupar la persistencia de progreso (`update_progress` en SQLite) mediante un regulador de tiempo (*throttle* de 1 segundo) para evitar saturar la base de datos con sentencias SQL `UPDATE` en cada paquete de red recibido.

---

### 1.4 Guardrails y Edge Cases Críticos

> [!CRITICAL]
> **Alineación de Renombrado Atómico (`.part` a archivo final)**:
> En Windows (`NTFS`), `tokio::fs::rename` falla con error de permisos si el archivo de destino ya existe o si el handle del archivo `.part` no ha sido cerrado por completo (`BufWriter::flush().await` y `drop(file)`). La tarea de descarga debe asegurarse de consumir y liberar explícitamente el writer antes de invocar la operación atómica de renombrado sobre el sistema de archivos.

> [!WARNING]
> **Enlaces Debrid con Expiración Rápida**:
> Si un enlace de Real-Debrid o TorBox devuelve `HTTP 403 Forbidden` o `HTTP 410 Gone` durante la reanudación, el sistema no debe marcar el trabajo como permanentemente fallido. Debe ejecutar la función de re-resolución de stream (`DownloadSource::resolve_fresh()`), obtener una nueva URL directa para el mismo infohash, y reanudar los bytes pendientes preservando el `.part` ya verificado.

---

## 2. Ejecutor Secuencial de `core-env` y Garantías FIFO en Estado de Core

### 2.1 Objetivos Técnicos y Métricas
- **Garantía absoluta de orden FIFO en `exec_sequential`**: Prevenir desorden en la ejecución de acciones de `stremio-core` que provoquen estados inconsistentes en la biblioteca del usuario o pérdidas de progreso de reproducción.
- **Cero bloqueos del pool de Tokio (Deadlock Immunity)**: Asegurar que la saturación de la cola secuencial no bloquee los hilos de trabajo del runtime asíncrono.
- **Rendimiento predecible bajo ráfagas**: Permitir la absorción elástica de eventos sin recurrir a la creación desordenada de hilos del sistema operativo (`spawn_blocking`).

---

### 2.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto: Ruptura del determinismo de orden por despachos en paralelo
En [`core-env/src/lib.rs:98-145`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/lib.rs#L98-L145):
```rust
fn submit_sequential(
    sender: &SyncSender<SequentialFuture>,
    future: SequentialFuture,
) -> Result<(), SequentialFuture> {
    match sender.try_send(future) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(future)) => {
            if Handle::try_current().is_ok() {
                // Cola saturada dentro de un hilo de Tokio:
                let sender_clone = sender.clone();
                spawn_on_runtime(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = sender_clone.send(future); // <-- CONDICIÓN DE CARRERA
                        SEQUENTIAL_SATURATED.store(false, Ordering::Release);
                    }).await;
                });
                Ok(())
            } else {
                sender.send(future).map_err(|error| error.0)
            }
        }
        Err(TrySendError::Disconnected(future)) => Err(future),
    }
}
```
**Causa Raíz:**
1. Cuando la capacidad acotada `SEQUENTIAL_QUEUE_CAPACITY` (64 elementos) se llena, si entran 5 acciones casi consecutivas (por ejemplo, 5 actualizaciones de progreso de episodios o selecciones rápidas), se lanzan 5 tareas asíncronas con Tokio y dentro 5 hilos bloqueantes con `spawn_blocking`.
2. Dado que los hilos del pool de bloqueo de Tokio compiten sin coordinación por acceder a `sender_clone.send(future)`, **el orden en que adquieren el canal es no determinista**.
3. Una acción que se despachó cronológicamente en tercer lugar puede entrar a la cola antes que la primera. Si la primera acción era `Action::LoadDetails` y la segunda era `Action::Unload`, el orden se invierte y la pantalla se queda cargando un recurso no deseado.
4. El flag atómico `SEQUENTIAL_SATURATED` se limpia prematuramente (`store(false)`) en cuanto el primer hilo que despierta logra meter su elemento, desincronizando la telemetría de saturación.

---

### 2.3 Arquitectura Objetivo y Especificación de Implementación

1. **Reemplazo por Cola Asíncrona Unificada o Executor FIFO Dedicado**:
   - Eliminar el esquema híbrido `SyncSender` + `spawn_blocking`.
   - Utilizar un canal de Tokio desacoplado con un búfer de alta capacidad o un actor consumidor secuencial con `tokio::sync::mpsc::unbounded_channel` (o cola acotada con semáforo de admisión FIFO).
   - Alternativamente, si se preserva el hilo dedicado para aislar las tareas que ejecutan FFI o código síncrono del core, el productor que detecta cola llena debe encolar en una estructura intermedia `Arc<Mutex<VecDeque<SequentialFuture>>>` protegida por orden estricto de inserción:
     ```rust
     struct SequentialExecutor {
         primary_tx: tokio::sync::mpsc::Sender<SequentialFuture>,
         // Cola de desborde con preservación estricta de orden:
         overflow: Arc<tokio::sync::Mutex<VecDeque<SequentialFuture>>>,
     }
     ```
2. **Backpressure Asíncrono Limpio**:
   - En lugar de disparar `spawn_blocking` arbitrarios, si un llamador asíncrono necesita despachar una tarea secuencial, invoca la variante asíncrona `sender.send(future).await`, la cual cede el control cooperativamente al scheduler de Tokio sin bloquear ningún hilo del sistema operativo.
3. **Monitoreo de Latencia de Cola**:
   - Registrar métricas de profundidad de cola y tiempo de estancia en cola (*queue residence time*). Si la cola supera 128 elementos durante más de 500 ms, emitir un warning estructurado en tracing con el desglose del tipo de acciones encoladas.

---

### 2.4 Guardrails y Edge Cases Críticos

> [!IMPORTANT]
> **Prevención de Deadlock con `Runtime::model()`**:
> Nunca debe ejecutarse una tarea dentro del ejecutor secuencial que espere activamente (bloqueo mutuo) a que el hilo principal de UI o un hilo de red termine de manipular un lock de `stremio-core`. Las tareas que entran a `exec_sequential` deben procesar el estado y salir en un tiempo acotado (< 5 ms por paso).

---

## 3. Eliminación de Condiciones de Carrera y Desincronización en el Preview Drawer

### 3.1 Objetivos Técnicos y Métricas
- **Cero inconsistencias en metadatos mostrados**: Imposibilidad de que datos de un medio previo sobrescriban los metadatos del medio seleccionado actualmente.
- **Ahorro de ancho de banda y CPU**: Cancelación inmediata de peticiones HTTP en vuelo (scraping de ratings en Rotten Tomatoes/Metacritic/Letterboxd y llamadas a YouTube para tráilers) al cambiar de selección.
- **Comportamiento idéntico en Discover y Library**: Apertura optimista instantánea sin omisión de sinopsis, año o estado de carga en ninguna de las páginas.

---

### 3.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto A: Falta de inicialización optimista en Library y Discover
En [`app/src/models/library.rs:265-275`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/library.rs#L265-L275):
Al hacer clic en una tarjeta en la biblioteca, se proyecta el título y el póster, pero **se omiten** `library_preview_description` y `library_preview_year`. Si el usuario tenía abierta la previsualización de una película anterior, el drawer mantiene la descripción y el año de la anterior mientras los nuevos metadatos llegan de la red.
En [`app/src/models/discover.rs:720-735`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/discover.rs#L720-L735):
Al navegar por primera vez a un catálogo, el sistema autoselecciona el primer ítem, pero no invoca `ui.set_discover_has_preview(true)`, dejando el panel en un estado híbrido donde hay datos cargados internamente pero el drawer está replegado (`width: 0px`).

#### Defecto B: Tareas huérfanas de ratings y carreras de red
En [`app/src/ratings.rs:92-136`](file:///home/kes/Documentos/Programando/stremio-native/app/src/ratings.rs#L92-L136):
```rust
pub fn fetch_and_apply_for_media(
    media_id: String,
    title: String,
    year: Option<u32>,
    media_type: String,
    ui_weak: slint::Weak<MainWindow>,
) {
    // Almacena el ID como último seleccionado:
    CURRENT_PREVIEW_MEDIA_ID.store(...);
    
    // Dispara tarea desacoplada sin retener JoinHandle ni AbortHandle:
    tokio::spawn(async move {
        // Hace scraping concurrente de IMDb, Rotten Tomatoes, Metacritic y Letterboxd:
        let ratings = fetch_all_ratings(&title, year, &media_type).await;
        
        // Verifica si el ID sigue siendo el actual:
        if CURRENT_PREVIEW_MEDIA_ID.load() == current {
            // Aplica a la UI
        }
    });
}
```
**Causa Raíz:**
1. Aunque existe una comprobación con `CURRENT_PREVIEW_MEDIA_ID`, **la tarea previa no se cancela**.
2. Si el usuario navega rápidamente con las flechas del teclado o hace clic sobre 10 tarjetas en 2 segundos, se lanzan 10 tareas concurrentes que realizan hasta 40 peticiones HTTP y parsing HTML simultáneo sobre sitios externos.
3. Esto consume hilos de Tokio, satura la red y provoca bloqueos temporales por detección de bots/scraping en Rotten Tomatoes y Letterboxd.

---

### 3.3 Arquitectura Objetivo y Especificación de Implementación

1. **Gestor de Selección con Cancelación Cooperativa (`SelectionTaskRegistry`)**:
   - Crear un controlador global de tareas en vuelo asociado a cada panel:
     ```rust
     struct PreviewTaskController {
         active_generation: AtomicU64,
         ratings_abort: Mutex<Option<tokio::task::AbortHandle>>,
         trailer_abort: Mutex<Option<tokio::task::AbortHandle>>,
     }
     ```
   - Al seleccionar un nuevo elemento, se incrementa atómicamente `active_generation` y se invoca `.abort()` sobre los handles previos. Las peticiones de red en curso se interrumpen de inmediato liberando sockets y memoria.
2. **Contrato de Identidad en Sincronización Slint (`Generation Guard`)**:
   - En [`app/src/models/details.rs:936-1111`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/details.rs#L936-L1111), cada callback hacia Slint debe recibir el `generation: u64` capturado en el momento de la solicitud.
   - Si `generation != controller.current_generation()`, la actualización se descarta antes de tocar cualquier propiedad de Slint.
3. **Paridad Total en Apertura Optimista**:
   - Unificar la función auxiliar `project_optimistic_drawer_state`:
     ```rust
     fn project_optimistic_drawer_state(ui: &MainWindow, card: &MediaCardItem, scope: DrawerScope) {
         match scope {
             DrawerScope::Discover => {
                 ui.set_discover_preview_id(card.id.clone().into());
                 ui.set_discover_preview_title(card.title.clone().into());
                 ui.set_discover_preview_year(card.year.unwrap_or(0) as i32);
                 ui.set_discover_preview_description(card.description.clone().into());
                 ui.set_discover_preview_poster(card.poster.clone());
                 ui.set_discover_preview_ratings_loading(true);
                 ui.set_discover_has_preview(true);
             }
             DrawerScope::Library => {
                 ui.set_library_preview_id(card.id.clone().into());
                 ui.set_library_preview_title(card.title.clone().into());
                 ui.set_library_preview_year(card.year.unwrap_or(0) as i32);
                 ui.set_library_preview_description(card.description.clone().into());
                 ui.set_library_preview_poster(card.poster.clone());
                 ui.set_library_preview_ratings_loading(true);
                 ui.set_library_has_preview(true);
             }
         }
     }
     ```

---

### 3.4 Guardrails y Edge Cases Críticos

> [!CAUTION]
> **Preservación del Tráiler en Reproducción**:
> Si el usuario ya está reproduciendo activamente un tráiler en el reproductor integrado del drawer, hacer clic en la misma tarjeta o recibir una actualización tardía de metadatos del core **no debe detener la reproducción ni reiniciar la sesión de MPV**. Debe verificarse si `current_preview_id == clicked_id` antes de invocar la recarga de la fuente multimedia.

> [!IMPORTANT]
> **Puntos de Cancelación y Parsing HTML Síncrono**:
> `tokio::task::AbortHandle::abort()` solo interrumpe la tarea cuando esta cede el control en un punto `.await`. Dado que el parsing del DOM / HTML de calificaciones (Rotten Tomatoes, Metacritic, Letterboxd) es intensivo en CPU y síncrono, la tarea **debe verificar `controller.current_generation() == task_generation` inmediatamente antes y después del parsing HTML**, y justo antes de emitir la llamada a `slint::invoke_from_event_loop`. Si la generación ya cambió, se descarta el resultado sin transferir trabajo inútil al hilo de UI.

---

## 4. Desbloqueo Radical del Hilo Gráfico de Slint (Zero-Jank UI Architecture)

### 4.1 Objetivos Técnicos y Métricas
- **Garantizar 60 / 120 FPS estables**: Cero caídas de cuadros (*frame drops / jank*) durante el scroll continuo o la sincronización de catálogos en el Board, Discover y Library.
- **Eliminar bloqueos por contención de Mutex en el hilo de UI**: Prohibir categóricamente llamadas a `runtime.model()` desde closures ejecutados dentro de `slint::invoke_from_event_loop`.
- **Reducción de memoria transitoria en recarga de interfaz**: Eliminar la clonación masiva de estructuras pesadas de `stremio-core` (evitar clonar `HashMap<String, LibraryItem>`).

---

### 4.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto: Re-adquisición de bloqueos de modelo y clonación profunda en el hilo de UI
En [`app/src/models/board.rs:318-375`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/board.rs#L318-L375):
```rust
// Se ejecuta DENTRO del hilo de UI de Slint:
pub fn sync(...) {
    // 1. Re-adquiere el RwLock del modelo de Stremio Core en el hilo de renderizado:
    if let Ok(model) = runtime.model() {
        // Lee configuración de advertencia de servidor...
    }
    
    // 2. Clona TODO el mapa de biblioteca del usuario:
    let library_items = runtime.model().ok().map(|m| m.ctx.library.items.clone());
    if let Some(lib) = library_items.as_ref() {
        for (id, item) in lib {
            fingerprint.str(id);
            fingerprint.bool(item.watched());
            fingerprint.bool(!item.removed);
        }
    }
    
    // 3. Vuelve a bloquear el modelo para extraer claves de streams:
    let saved_stream_keys = runtime.model().ok().map(|model| {
        model.ctx.streams.items.keys().cloned().collect::<HashSet<_>>()
    }).unwrap_or_default();
```
**Causa Raíz:**
1. Aunque [`app/src/event_loop.rs:604-620`](file:///home/kes/Documentos/Programando/stremio-native/app/src/event_loop.rs#L604-L620) clona los submodelos en un hilo secundario para liberar el lock antes de programar la invocación gráfica, las implementaciones de `models::board::sync`, `models::discover::sync` y `models::library::sync` **vuelven a solicitar `runtime.model()` reiteradamente dentro del hilo de renderizado**.
2. Si un hilo de fondo de `stremio-core` está procesando una mutación pesada (por ejemplo, guardando el perfil o deserializando una respuesta de addon), el hilo principal de Slint queda **congelado** esperando el `RwLock`.
3. Clonar un `HashMap` con cientos o miles de `LibraryItem` (que contienen cadenas, metadatos, listas de episodios y flags) genera cientos de micro-asignaciones en el heap en pleno frame de renderizado, activando el recolector o disparando contención del allocator.

---

### 4.3 Arquitectura Objetivo y Especificación de Implementación

```
[ Worker Tokio (Fondo) ]
          │
          ├─► Adquiere `runtime.model()` (fuera del hilo de UI)
          ├─► Calcula Snapshot Proyectado Ligero (Plain Data DTO)
          │     ├── `server_warning_active: bool`
          │     ├── `watched_ids: HashSet<String>`
          │     ├── `saved_stream_ids: HashSet<String>`
          │     └── `cards: Vec<MediaCardItem>` (pre-construidas)
          ├─► Calcula Fingerprint de 64 bits (XXHash / Murmur3)
          ├─► Verifica si cambió respecto al último frame
          │     └── Si NO cambió: Descarta y NO molesta a Slint.
          │
          └─► Si cambió: `slint::invoke_from_event_loop(move || {`
                     [ Hilo de UI de Slint ]
                     - Cero llamadas a `runtime.model()`
                     - Cero clonaciones de colecciones de Core
                     - Aplicación directa de los vectores a los `ModelRc`
```

1. **Estructura DTO de Proyección Desacoplada (`BoardPresentationSnapshot`)**:
   ```rust
   pub struct BoardPresentationSnapshot {
       pub show_server_warning: bool,
       pub continue_watching_cards: Vec<MediaCardItem>,
       pub catalog_rows: Vec<CatalogRowModel>,
       pub fingerprint: u64,
   }
   ```
2. **Computación 100% Fuera del Hilo de UI**:
   - Toda la transformación de tipos de `stremio-core` a tipos de presentación se efectúa en el hilo de trabajo de Tokio.
   - El hilo gráfico de Slint se limita exclusivamente a asignar los modelos Slint pre-calculados (`ui.set_continue_watching_model(...)`).
3. **Optimización Idéntica en Discover y Library**:
   - Replicar este desacoplamiento estricto en [`models::discover::sync`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/discover.rs#L510-L545) y [`models::library::sync`](file:///home/kes/Documentos/Programando/stremio-native/app/src/models/library.rs#L415-L420).

---

### 4.4 Guardrails y Edge Cases Críticos

> [!IMPORTANT]
> **Prohibición de `runtime.model()` en Callbacks de Slint**:
> Ningún callback registrado mediante `ui.on_<action>` debe invocar `.unwrap()` sobre `runtime.model()`. Cualquier acción disparada por el usuario (como agregar a favoritos, marcar visto, o abrir opciones) debe despachar un evento asíncrono o un `Action` al runtime del core y retornar inmediatamente para mantener la interfaz receptiva.

---

## 5. Regulación de Concurrencia en Verificaciones Debrid y Prevención de Rate-Limits (HTTP 429)

### 5.1 Objetivos Técnicos y Métricas
- **Cero bloqueos por HTTP 429 Too Many Requests**: Eliminar la sobrecarga en las APIs de Real-Debrid, AllDebrid, TorBox y Premiumize.
- **Tiempos de respuesta percibidos más rápidos**: Mostrar disponibilidad instantánea para los primeros streams evaluados sin esperar a que finalice la verificación de toda la lista de torrents.
- **Consumo de ancho de banda y conexiones de socket ordenado**: Limitar la concurrencia a un máximo predecible de conexiones simultáneas.

---

### 5.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto: Disparo simultáneo ilimitado mediante `futures::future::join_all`
En [`app/src/event_loop.rs:132-161`](file:///home/kes/Documentos/Programando/stremio-native/app/src/event_loop.rs#L132-L161):
```rust
let checks = candidates.into_iter().map(|(selection_id, hash)| {
    let providers = providers.clone();
    async move {
        let results = futures::future::join_all(
            providers.iter().map(|provider| provider.availability(&hash)),
        ).await;
        // Mapeo de disponibilidad...
        available.map(|available| (selection_id, available))
    }
});
let availability = futures::future::join_all(checks) // <-- EXPLOSIÓN DE CONCURRENCIA
    .await
    .into_iter()
    .flatten()
    .collect();
```
**Causa Raíz:**
1. Si un usuario busca una película popular o un episodio de serie con múltiples addons de torrents activos (Torrentio, MediaFusion, Comet, etc.), la lista de candidatos puede contener entre **60 y 120 streams únicos**.
2. Si hay 2 proveedores debrid configurados (por ejemplo, Real-Debrid y TorBox), el código actual genera un vector de más de 200 futuros y los ejecuta **al unísono en el mismo milisegundo**.
3. Real-Debrid implementa un límite estricto de ráfaga (*rate limit* por IP de ~25-50 req/min). Al recibir decenas de llamadas concurrentes para consultar `/torrents/instantAvailability/{hash}`, el servidor responde de inmediato con `HTTP 429 Too Many Requests` o denegaciones temporales de Cloudflare.
4. Consecuencia: Streams que sí estaban cacheados y listos para reproducir al instante se marcan erróneamente como `Uncached` o no disponibles, obligando al usuario a iniciar descargas lentas o pensar que su suscripción no funciona.

---

### 5.3 Arquitectura Objetivo y Especificación de Implementación

1. **Control de Concurrencia con `StreamExt::buffer_unordered`**:
   - Reemplazar el `join_all` incondicional por un pipeline de streams con concurrencia máxima acotada (por ejemplo, 4 a 6 peticiones simultáneas):
     ```rust
     use futures::stream::{self, StreamExt};
     
     const MAX_CONCURRENT_DEBRID_CHECKS: usize = 4;
     
     let results = stream::iter(checks)
         .buffer_unordered(MAX_CONCURRENT_DEBRID_CHECKS)
         .collect::<Vec<_>>()
         .await;
     ```
2. **Batching Nativo de Hashes por Proveedor**:
   - Aprovechar las capacidades multi-hash de los proveedores compatibles:
     - Real-Debrid soporta la concatenación de múltiples hashes en una sola URL (`/torrents/instantAvailability/{hash1}/{hash2}/...`).
     - TorBox y AllDebrid soportan endpoints por lotes en sus APIs v2.
   - En lugar de realizar 80 peticiones HTTP individuales, agrupar los hashes en lotes de 25 elementos, reduciendo el volumen de peticiones en un **96%** (de 80 peticiones a solo 3 o 4).
3. **Proyección Progresiva a la Interfaz**:
   - En lugar de esperar a que terminen los 120 streams para ejecutar una única llamada masiva a `playback_selections.rebuild()`, emitir actualizaciones parciales cada vez que un lote de 10 elementos se resuelve, permitiendo al usuario hacer clic en el primer stream cached que aparezca en pantalla.

---

### 5.4 Guardrails y Edge Cases Críticos

> [!TIP]
> **Caché en Memoria de Disponibilidad de Infohashes**:
> Dado que la disponibilidad de un torrent en la nube de Debrid no cambia de estado en cuestión de segundos, los resultados verificados deben almacenarse en una caché LRU en memoria (`moka::sync::Cache<[u8; 20], DebridAvailability>`) con un TTL de 10 minutos. Si el usuario vuelve a abrir la lista de streams tras salir momentáneamente, la verificación debe resolverse en 0 ms sin consultar la red.

---

## 6. Optimización de E/S y Hashing Incremental en el Escáner de Librería Local

### 6.1 Objetivos Técnicos y Métricas
- **Reducción de E/S de disco en un 95%+ durante re-escaneos**: Eliminar la relectura innecesaria de archivos y el recálculo masivo de hashes BLAKE3 cuando los archivos no han sufrido modificaciones.
- **Inmunidad ante falsos disparos del Watcher**: Ignorar eventos de solo lectura o accesos cuando el reproductor abre un archivo para visualización.
- **Experiencia de usuario instantánea en bibliotecas masivas**: Permitir que colecciones de 2.000 a 10.000 archivos en discos duros tradicionales (HDD) o almacenamiento conectado en red (NAS) se sincronicen en menos de 1 segundo tras el inicio.

---

### 6.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto A: Disparo ciego ante cualquier evento del sistema de archivos
En [`app/src/local_library.rs:181-188`](file:///home/kes/Documentos/Programando/stremio-native/app/src/local_library.rs#L181-L188):
```rust
event = events_rx.recv() => {
    if event.is_none() { break; }
    tokio::time::sleep(Duration::from_millis(750)).await;
    while events_rx.try_recv().is_ok() {}
    let _ = manager.scan(root.clone()).await; // <-- RE-ESCANEO COMPLETO
}
```
**Causa Raíz:** `notify::Event` genera eventos para **cualquier** interacción, incluyendo `EventKind::Access(AccessKind::Open)` y `AccessKind::Close` cuando el usuario reproduce una película local en MPV. Al abrir el archivo para mirar el video, el watcher detecta el acceso y dispara un escaneo completo de todo el disco.

#### Defecto B: Hashing Blake3 y escaneo de subtítulos incondicional
En [`app/src/local_library.rs:570-610`](file:///home/kes/Documentos/Programando/stremio-native/app/src/local_library.rs#L570-L610):
```rust
fn inspect_media_file(root: &LocalRoot, path: &Path) -> Result<LocalMediaItem, LocalLibraryError> {
    let metadata = path.metadata().map_err(|_| LocalLibraryError::Scan)?;
    let size_bytes = metadata.len();
    let modified_at = ...;
    
    // 1. Abre el archivo, hace seek al inicio y al final, lee 128 KB y calcula Blake3:
    let fingerprint = fingerprint(path, size_bytes)?;
    
    // 2. Parsea strings de nombre de archivo:
    let parsed = parse_filename(path);
    
    // 3. Busca y lee archivos .nfo en disco:
    let nfo = read_nfo(path);
    
    // 4. Hace un `read_dir` en el directorio contenedor buscando archivos .srt / .vtt:
    let subtitles = discover_subtitles(path);
    ...
```
**Causa Raíz:** Para cada uno de los archivos encontrados por el `WalkDir`, el sistema abre el archivo en disco, ejecuta lecturas para el fingerprint, lee archivos NFO y lista directorios buscando subtítulos, **incluso si el archivo ya fue escaneado previamente y su tamaño y fecha de modificación no han cambiado ni un solo byte**.

---

### 6.3 Arquitectura Objetivo y Especificación de Implementación

1. **Filtrado Semántico de Eventos del File Watcher**:
   - En el receptor de eventos de `notify`, descartar inmediatamente cualquier evento que no sea de tipo `EventKind::Create(_)`, `EventKind::Modify(ModifyKind::Data(_))`, `EventKind::Modify(ModifyKind::Name(_))` o `EventKind::Remove(_)`.
   - Ignorar eventos sobre extensiones que no correspondan a medios o metadatos (`.mkv`, `.mp4`, `.avi`, `.mov`, `.srt`, `.vtt`, `.nfo`).
2. **Estrategia de Escaneo Incremental Basada en Índice (Metadata Fast-Path)**:
   - Antes de iniciar el recorrido, cargar desde SQLite un mapa en memoria de los ítems existentes para esa raíz:
     `HashMap<PathBuf, (u64 /* size */, i64 /* modified_at */, LocalMediaItem)>`.
   - Durante la iteración de `WalkDir`, consultar únicamente la metadata del sistema de archivos (`entry.metadata()`):
     ```rust
     if let Some(existing) = cached_index.get(path) {
         let mtime_diff = (existing.modified_at - modified_at).abs();
         // Tolerancia de ±2 segundos para tolerar redondeos en SMB/NFS, FAT32 y exFAT:
         if existing.size_bytes == size_bytes && mtime_diff <= 2 {
             // FAST PATH: El archivo no fue modificado.
             // Reutilizar el fingerprint, metadata, NFO y subtítulos ya calculados.
             items.push(existing.clone());
             continue;
         }
     }
     // SLOW PATH: Archivo nuevo o modificado -> Ejecutar inspect_media_file completo.
     ```
3. **Actualización Atómica y Detección de Eliminados**:
   - Los ítems del índice previo que ya no existan en el disco se eliminan de la base de datos en una sola sentencia SQL transaccional (`DELETE FROM local_media_items WHERE id IN (...)`).

---

### 6.4 Guardrails y Edge Cases Críticos

> [!WARNING]
> **Subtítulos Externos Agregados Posteriormente**:
> Si un usuario agrega un archivo de subtítulo `pelicula.es.srt` en la misma carpeta sin modificar el archivo de video `pelicula.mkv`, el `mtime` del video no cambia.
> **Regla**: Cuando el watcher detecte la adición de un archivo con extensión `.srt` o `.vtt`, debe invalidar el registro de los videos adyacentes en ese mismo subdirectorio para forzar la actualización de su lista de subtítulos disponibles.

---

## 7. Escalabilidad de Base de Datos SQLite/Turso y Desacople de Caché HTTP

### 7.1 Objetivos Técnicos y Métricas
- **Eliminación del cuello de botella global en `get_db_conn()`**: Permitir lecturas concurrentes sin esperas en el modo WAL de SQLite.
- **Reducción de latencia en consultas repetidas de catálogos y metadatos**: Servir el 80%+ de las peticiones de caché HTTP directamente desde memoria RAM (< 1 ms).
- **Reducción de contención de escritura en disco**: Agrupar las sentencias `UPDATE accessed_at` evitando transacciones individuales innecesarias.

---

### 7.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto: Serialización absoluta mediante un único `AsyncMutex<()>`
En [`core-env/src/lib.rs:299-300`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/lib.rs#L299-L300) y [`core-env/src/lib.rs:530-537`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/lib.rs#L530-L537):
```rust
static DB_CONNECTION_ACCESS: LazyLock<Arc<AsyncMutex<()>>> =
    LazyLock::new(|| Arc::new(AsyncMutex::const_new(())));

pub(crate) async fn get_db_conn() -> Result<DbConn, EnvError> {
    let guard = DB_CONNECTION_ACCESS.clone().lock_owned().await; // <-- SERIALIZADOR GLOBAL
    if let Some(conn) = read_database_slot(&DB_CONNECTION) {
        return Ok(DbConn { conn, _guard: guard });
    }
```
**Causa Raíz:**
1. Para evitar que dos llamadas concurrentes a `turso::Connection` compartan el estado de transacciones `BEGIN`/`COMMIT`, se envolvió la conexión en un `AsyncMutex<()>` global (`DB_CONNECTION_ACCESS`).
2. Consecuencia: **Todas las operaciones contra la base de datos están estrictamente serializadas**.
3. Cada lectura de caché HTTP ([`http_cache::read_entry`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/http_cache.rs#L406-L429)), cada escritura de respuesta ([`http_cache::write_response`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/http_cache.rs#L431-L479)), cada actualización en segundo plano de telemetría de acceso ([`schedule_access_touch`](file:///home/kes/Documentos/Programando/stremio-native/core-env/src/http_cache.rs#L481-L498)), y cada lectura de perfil/almacenamiento debe esperar en la misma cola detrás del mutex.
4. Cuando se cargan múltiples catálogos de addons en Discover o en el Board, decenas de tareas asíncronas se bloquean secuencialmente esperando el turno del lock, provocando picos de latencia de cientos de milisegundos en peticiones que deberían resolverse de inmediato.

---

### 7.3 Arquitectura Objetivo y Especificación de Implementación

1. **Caché en Memoria L1 para HTTP Cache (Moka Front-Cache)**:
   - Integrar un caché en memoria LRU (`moka::future::Cache<String, Arc<HttpCacheEntry>>`) antes de golpear la base de datos SQLite.
   - Configuración: Capacidad máxima de 2.000 entradas y TTL configurable (por ejemplo, 15 minutos para catálogos y 1 hora para metadatos fijos).
   - Flujo de lectura:
     ```
     Petición HTTP Cache
            │
            ├─► ¿Existe en Moka Cache (RAM)?
            │     ├── SÍ: Retornar en < 50 µs (Cero llamadas a SQLite, cero locks).
            │     └── NO: Consultar SQLite, popular Moka Cache y retornar.
     ```
2. **Gestión Pragmática de Conexiones Turso / LibSQL**:
   - `libsql` localmente sobre WAL comparte estados de bloqueo internos entre estructuras `Connection` en el mismo proceso. Por tanto, la introducción de un pool multiconexión puede resultar redundante o contraproducente en ciertas versiones del driver.
   - **Estrategia prioritaria**: El front-cache L1 en memoria (Moka) y la agrupación (*debounce*) de escrituras de `accessed_at` absorberán más del 92% de las peticiones a `http_cache`. Esto reduce la contención sobre el mutex de Turso a niveles insignificantes sin necesidad de introducir complejidad de pooling frágil.
3. **Agrupación en Lote (Debounce) de `accessed_at`**:
   - En lugar de despachar una tarea asíncrona y una conexión SQL por cada toque de acceso en `schedule_access_touch`, encolar las claves en un canal desacoplado `tokio::sync::mpsc::channel(1024)`.
   - Un worker en segundo plano vacía el canal cada 5 segundos y ejecuta un único comando masivo:
     `UPDATE http_cache SET accessed_at = ? WHERE cache_key IN (?, ?, ...)`.

---

### 7.4 Guardrails y Edge Cases Críticos

> [!CAUTION]
> **Consistencia de Inserción y Reemplazo**:
> Cuando `write_response` o `delete_entry` muten una entrada en SQLite, deben invalidar atómicamente la clave correspondiente en la caché L1 de Moka (`cache.invalidate(&key).await`) para prevenir la lectura de datos obsoletos en caso de refrescos forzados o re-autenticación de addons.

---

## 8. Ciclo de Vida y Evicción de Memoria en Instancias Secundarias de `libmpv`

### 8.1 Objetivos Técnicos y Métricas
- **Reducción de hasta 80 MB de memoria residente (RSS)**: Liberar instancias ociosas de `libmpv`, decodificadores de FFmpeg y buffers de renderizado OpenGL/Vulkan cuando no se utilicen.
- **Cero fugas de hilos de fondo del sistema operativo**: Asegurar que los hilos dedicados de preview y thumbnail finalicen ordenadamente durante periodos prolongados de inactividad.
- **Reactivación transparente sin parpadeo**: Capacidad de re-instanciar el reproductor bajo demanda en menos de 100 ms cuando el usuario vuelva a interactuar.

---

### 8.2 Diagnóstico a Bajo Nivel y Estado Previo

#### Defecto: Instancias de `libmpv` residentes de por vida una vez inicializadas
En [`app/src/preview_player.rs:81-95`](file:///home/kes/Documentos/Programando/stremio-native/app/src/preview_player.rs#L81-L95) y [`app/src/thumbnail_preview.rs:140-157`](file:///home/kes/Documentos/Programando/stremio-native/app/src/thumbnail_preview.rs#L140-L157):
```rust
pub fn ensure_started(&self) {
    let starter = {
        let mut guard = self.starter.lock().unwrap_or_else(|p| p.into_inner());
        guard.take() // <-- CONSUME EL STARTER DE FORMA DEFINITIVA
    };
    if let Some(starter) = starter {
        if let Some(runtime) = starter(self) {
            *self.runtime.lock().unwrap_or_else(|p| p.into_inner()) = Some(runtime);
        }
    }
}
```
**Causa Raíz:**
1. El método `ensure_started()` extrae (`guard.take()`) la clausura de inicio del runtime (`PreviewStarter`).
2. Una vez que el usuario pasa el mouse sobre una tarjeta con tráiler o pasa el cursor sobre la barra de búsqueda de tiempo (*scrubber*) en el reproductor principal, se arrancan los runtimes secundarios de MPV (`PreviewRuntime` y `ThumbnailRuntime`).
3. Ambos runtimes ejecutan hilos nativos dedicados con bucles de eventos bloqueantes y retienen contextos de decodificación de hardware de la GPU y bibliotecas dinámicas en RAM.
4. Aunque el usuario cierre el drawer o reproduzca una película completa en pantalla completa durante 3 horas, **estas instancias secundarias continúan consumiendo entre 30 MB y 80 MB de memoria de trabajo indefinidamente**.

---

### 8.3 Arquitectura Objetivo y Especificación de Implementación

1. **Patrón de Fábrica Reutilizable (Persistent Factory)**:
   - Modificar la definición de `starter` para que no sea un `FnOnce`, sino un `Arc<dyn Fn() -> Option<Runtime> + Send + Sync>` capaz de invocar la creación de nuevas instancias múltiples veces.
2. **Estrategia Diferenciada de Evicción por Inactividad (Lifecycle Watchdog)**:
   - **`PreviewRuntime` (Tráilers en Drawer de Catálogo)**:
     - Mantiene un temporizador de inactividad de 3 minutos tras cerrarse el drawer. Si no hay interacción, se invoca `runtime.shutdown()` y se libera la instancia a `None`, liberando 30–50 MB de memoria en sesiones largas de navegación.
   - **`ThumbnailRuntime` (Previsualización de scrubber en Línea de Tiempo)**:
     - Destruir e inicializar decodificadores de hardware (VAAPI, NVDEC, D3D11) en plena reproducción de una película introduce latencia perceptible, tirones de audio o micro-parpadeos en el driver de video.
     - **Regla**: El ciclo de vida de `ThumbnailRuntime` se vincula estrictamente a la sesión del reproductor principal: permanece activo mientras el usuario esté viendo el contenido (incluso si no hace hover sobre la barra durante varios minutos), y se destruye ordenadamente en el momento exacto en que el usuario detiene la reproducción y regresa al catálogo (`unload_player` / cierre de sesión de video).
3. **Reactivación JIT (Just-In-Time)**:
   - Cuando el usuario vuelva a abrir el drawer o vuelva a situar el cursor sobre la línea de tiempo, `ensure_started()` detecta que `self.runtime` es `None` y reconstruye la instancia de forma transparente.

---

### 8.4 Guardrails y Edge Cases Críticos

> [!IMPORTANT]
> **Coexistencia y Prevención de Conflictos de Contextos Gráficos**:
> Al destruir e inicializar repetidamente instancias de `libmpv` en plataformas Windows/DirectX o Linux/EGL, debe verificarse que los recursos de renderizado compartidos (handles de texturas o contextos de render de renderizado de software) se liberen antes de que el nuevo contexto intente asociarse a la misma ventana de Slint.

---

## 9. Matriz Comparativa para Auditoría y Plan de Ejecución

### 9.1 Matriz Resumen de Diagnóstico y Objetivos

| Área / Subsistema | Cómo Estaba (Defecto Previo) | Cómo Va a Quedar (Objetivo) | Restricción Crítica / Guardrail |
| :--- | :--- | :--- | :--- |
| **1. Gestor de Descargas** | `.timeout(30s)` aborta descargas largas; `.part` se reanuda sin validar `If-Range` ni `ETag`. | Timeout de inactividad por chunk (45s); validación estricta de `If-Range`/`Content-Range` y búfer en disco de 512 KB. | Truncar archivo a 0 si el servidor responde `200 OK` en vez de `206`. Cerrar handles antes de renombrar en Windows. |
| **2. Ejecutor Secuencial Core** | Despachos con `spawn_blocking` en desborde rompen el orden FIFO de acciones en el Core. | Canal asíncrono con backpressure cooperativo y preservación estricta del orden de llegada. | Cero llamadas bloqueantes o retención de locks cruzados dentro de las tareas del Core. |
| **3. Preview Drawer** | Descripciones no inicializadas en Library; scraping de ratings lanza tareas huérfanas sin abortar. | Identidad estricta por generación (`active_generation`); cancelación cooperativa (`AbortHandle`) al cambiar de tarjeta. | No interrumpir la reproducción del tráiler activo si el usuario hace clic sobre el mismo ítem. |
| **4. Hilo Gráfico Slint** | Re-adquisición de `runtime.model()` y clonación profunda de `library.items` en el hilo de renderizado (Jank). | Proyección 100% desacoplada en hilo Tokio de fondo; Slint solo recibe DTOs inmutables listos para pintar (60/120 FPS). | Prohibir llamadas síncronas a `runtime.model()` en callbacks de eventos de Slint. |
| **5. Verificaciones Debrid** | `join_all` simultáneo de más de 100 streams satura APIs y provoca bloqueos por HTTP 429. | Concurrencia acotada con `buffer_unordered(4)`, agrupación por lotes (*batching*) y caché LRU de hashes de 10 min. | Proyectar resultados incrementalmente a la UI sin esperar a la finalización de toda la lista. |
| **6. Librería Local** | Re-escaneo completo ante cualquier evento de lectura; hashing BLAKE3 y parsing NFO en cada pasada. | Filtrado de eventos mutantes en `notify`; omitir hashing si `(path, size, mtime)` no cambiaron en base de datos. | Invalidar caché de archivos multimedia adyacentes si se agrega un nuevo archivo de subtítulos externos (`.srt`). |
| **7. Concurrencia SQLite** | `DB_CONNECTION_ACCESS: AsyncMutex<()>` serializa todas las lecturas de caché HTTP y storage. | Moka front-cache en memoria RAM para entradas calientes; agrupación (*debounce*) de escrituras de `accessed_at`. | Invalidar atómicamente la entrada en memoria cuando ocurra un `write_response` o `delete_entry`. |
| **8. Ciclo de Vida libmpv** | Instancias de preview y thumbnails residen permanentemente en RAM tras su primer uso (~80 MB). | Temporizador de inactividad (3 min); apagado ordenado de instancias secundarias y reactivación bajo demanda. | Asegurar liberación total de handles de renderizado antes de reinicializar para no colisionar con Slint. |

---

### 9.2 Plan de Ejecución Ordenado por Fases

El despliegue de estas optimizaciones debe realizarse en cuatro fases incrementales, garantizando la estabilidad del sistema mediante pruebas unitarias y de integración en cada etapa:

```
┌────────────────────────────────────────────────────────┐
│ Fase 1: Fiabilidad Crítica de Red y Estado             │
│ - Reparación de timeouts y reanudación en descargas    │ (Sección 1)
│ - Estabilización de orden FIFO en el Core              │ (Sección 2)
└──────────────────────────┬─────────────────────────────┘
                           │
┌──────────────────────────▼─────────────────────────────┐
│ Fase 2: Fluidez de Interfaz y Prevención de Carreras   │
│ - Desbloqueo del hilo gráfico de Slint (Zero-Jank)    │ (Sección 4)
│ - Control de generación y cancelación en Preview Drawer│ (Sección 3)
└──────────────────────────┬─────────────────────────────┘
                           │
┌──────────────────────────▼─────────────────────────────┐
│ Fase 3: Concurrencia de Red y Escalabilidad de E/S     │
│ - Regulación de concurrencia Debrid (anti-429)         │ (Sección 5)
│ - Escaneo incremental de librería local (mtime fastpath)│ (Sección 6)
└──────────────────────────┬─────────────────────────────┘
                           │
┌──────────────────────────▼─────────────────────────────┐
│ Fase 4: Optimización de Recursos y Memoria             │
│ - Front-cache L1 y pool concurrente en SQLite          │ (Sección 7)
│ - Evicción por inactividad de instancias secundarias   │ (Sección 8)
└────────────────────────────────────────────────────────┘
```
