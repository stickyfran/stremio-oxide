# Roadmap de Resiliencia, Anti-Bloqueo DoH y Notificaciones Flotantes (RoadmapConsistency.md)

Este documento detalla la arquitectura definitiva para blindar **Stremio Oxide / Native** contra bloqueos de ISPs, envenenamiento de DNS, fallos de red y desincronizaciones de UI, incorporando las mejores prácticas de bajo consumo, deduplicación de eventos y experiencia de usuario fluida y transparente.

---

## 1. Arquitectura General del Sistema

```mermaid
flowchart TD
    subgraph Módulo A: Anti-Bloqueo & DoH Inteligente (core-env)
        Req[Petición reqwest] --> Resolve[reqwest::dns::Resolve personalizado]
        Resolve --> SysDNS[Resolver del Sistema OS]
        SysDNS --> CheckIP{¿IP Global y Válida?}
        CheckIP -- Sí --> Cache[Guardar en Cache DashMap]
        CheckIP -- No: Bogo-IP (127.x, 10.x, 192.168.x, 100.64.x, NXDOMAIN) --> DoH[Hickory DNS-over-HTTPS Cloudflare/Google]
        DoH --> LogBypass[Registrar Bypass y Guardar en Cache]
        LogBypass --> NotifyISP[Disparar Toast Deduplicado: Bloqueo Evitado]
        Cache --> Conn[Conexión TCP / TLS]
        Conn --> CheckDPI{¿TCP RST / Timeout post-DoH?}
        CheckDPI -- Sí --> ToastDPI[Toast: Bloqueo Profundo / DPI detectado]
    end

    subgraph Módulo B: Watchdog Acotado & Anti-Flicker (board.rs)
        InitApp[Inicio de la App / F5] --> WD[Watchdog acotado: máx 2 intentos a 4s y 10s]
        WD --> CheckBoard{¿Filas 0..3 vacías o en error?}
        CheckBoard -- Sí --> UnloadPreserve[Despachar Unload manteniendo VecModel visual previo]
        UnloadPreserve --> ForceLoad[LoadRange 0..4 forzado]
        CheckBoard -- Fallo 2do intento --> ToastBoard[Toast con acción manual 'Reintentar']
    end

    subgraph Módulo C: Metadatos Inmediatos & Streams Granulares (details.rs)
        ClickCard[Click en Tarjeta] --> Prefill0ms[Inyectar MetaItemPreview en Slint t=0ms]
        Prefill0ms --> FetchStreams[Core solicita Streams a Addons]
        FetchStreams --> CheckAddonStreams{¿Falló Torrentio u otro?}
        CheckAddonStreams -- Solo falló 1 addon --> InlineBanner[Fila compacta en lista de streams: 'Fallo Torrentio - Reintentar']
        CheckAddonStreams -- Addons restantes OK --> ShowAvailableStreams[Mostrar streams disponibles con normalidad]
    end

    subgraph Módulo D: ToastManager Flotante No Invasivo (Slint + Tokio)
        TriggerToast[Cualquier hilo Tokio emite send_toast] --> Dedup[Deduplicador por dedup_key + Coalescencia]
        Dedup --> SlintDispatch[slint::invoke_from_event_loop]
        SlintDispatch --> ToastUI[FloatingToastSquircle abajo a la derecha]
        ToastUI --> UserAction{Acción del usuario}
        UserAction -- Clic en 'X' --> DismissToast[Descartar inmediatamente]
        UserAction -- Timeout 6s (Timer Slint con pausa hover) --> DismissToast
        UserAction -- Clic en Botón --> ActionEnum[ToastAction enum ligero: RetryBoard, RetryStreams, etc.]
    end
```

---

## 2. Especificación Técnica Detallada

### 2.1 Módulo A: Anti-Bloqueo DoH con Filtro Bogo-IP y Detección DPI (`core-env`)
* **Ubicación:** `core-env/src/dns.rs` y conexión en `core-env/src/lib.rs`.
* **Implementación con `reqwest::dns::Resolve`:**
  - Se implementa el trait `Resolve` de `reqwest` utilizando un resolver dual:
    1. Primero consulta la resolución estándar del sistema.
    2. **Filtro de Bogo-IPs:** Si el dominio consultado no es local (`.local`, `.lan`, `localhost`) y resuelve a cualquier dirección que **no sea enrutable globalmente**:
       - Loopback: `127.0.0.0/8`
       - Bloques privados RFC 1918: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
       - CGNAT: `100.64.0.0/10`
       - Link-local o comodín: `0.0.0.0`, `169.254.0.0/16`
       - O si el resolver local devuelve `NXDOMAIN` para un servicio público conocido.
    3. Si se detecta cualquiera de estos casos, se conmuta en caliente a **DNS-over-HTTPS (DoH)** consultando `https://cloudflare-dns.com/dns-query` (o `1.1.1.1` directo) y `https://dns.google/dns-query`.
  - **Caché en Memoria:** Las IPs resueltas por DoH se guardan en un `DashMap<String, (Vec<SocketAddr>, Instant)>` con TTL respetando el DNS, evitando consultar DoH en cada request subsecuente.
  - **Detección de Bloqueo DPI (SNI):** Si a pesar de tener la IP válida de DoH la conexión TLS sufre un `Connection Reset by Peer` inmediato (TCP RST durante el ClientHello), el error se clasifica específicamente para que el Toast notifique al usuario: *"Tu proveedor de internet está aplicando bloqueo profundo (DPI) en este addon"*.
  - **Notificación Única de Bypass Exitoso:** La primera vez que el DoH esquiva con éxito un bloqueo de ISP en la sesión, emite un aviso silencioso o Toast informativo positivo: *"Bloqueo de tu proveedor evitado automáticamente vía DNS seguro"*.

---

### 2.2 Módulo B: Watchdog Acotado y Recarga sin Parpadeo (`board.rs`)
* **Ubicación:** `app/src/models/board.rs`, `app/src/callbacks.rs`.
* **Prevención de Bucles Infinitos:**
  - El watchdog de auto-curación se limita a un **máximo estricto de 2 intentos**:
    - Primer chequeo: a los 4 segundos del arranque.
    - Segundo chequeo (con backoff): a los 10 segundos.
  - Si en el segundo intento Cinemeta sigue sin responder (por ejemplo, si no hay internet o Cinemeta cayó globalmente), el watchdog se detiene y emite un Toast con botón manual *"Reintentar"*.
* **Preservación Visual (Anti-Flicker) en F5:**
  - Para evitar que la interfaz vacíe el `VecModel` de Slint y cause un salto visual molesto durante el `Action::Unload`, el modelo visual conserva los elementos previos mientras se despacha el refresco en segundo plano y solo se actualiza cuando los nuevos datos están listos o en transición.
  - Al presionar **F5**, se dispara `Action::Unload` seguido de `queue_visible_catalog(loader, 0)` para garantizar que `stremio-core` no descarte la acción por igualdad de estado (`eq_update`).

---

### 2.3 Módulo C: Metadatos Inmediatos y Granularidad por Addon (`details.rs`)
* **Ubicación:** `app/src/models/details.rs`.
* **Inyección Inmediata ($t = 0\text{ ms}$):**
  - Al pulsar una película o serie, se cargan instantáneamente en las propiedades de Slint el título, afiche, fondo y año ya conocidos desde la tarjeta (`MetaItemPreview`).
* **Manejo Granular de Streams:**
  - En lugar de bloquear o poner en error toda la pantalla si falla un addon como Torrentio, si otros addons responden con streams válidos, estos se muestran inmediatamente.
  - Si un addon específico falla por red o bloqueo, se muestra una fila de alerta compacta e intuitiva arriba de la lista de fuentes:
    > ⚠️ *No se pudo conectar a Torrentio. [Reintentar]*

---

### 2.4 Módulo D: Sistema de Notificaciones Toast Flotante (`ToastManager` + Slint)
* **Ubicación:** `app/src/toast_manager.rs`, `app/ui/common/toast.slint`, `app/ui/app.slint`.
* **Diseño Visual en Slint:**
  - **Estilo:** Tarjeta compacta rectangular con curvatura suave (`border-radius: 14px`), borde sutil translúcido (`1px solid rgba(255, 255, 255, 0.12)`), fondo oscuro `rgba(18, 18, 24, 0.92)` con sombra de elevación.
  - **Posición:** Flotando en la esquina inferior derecha de la ventana (`z: 1000`).
  - **Controles del Toast:**
    - Botón de cierre **`X`** circular arriba a la derecha.
    - Botón de acción rápida opcional según la acción recomendada (ej. *"Reintentar"*).
    - Barra de temporizador inferior de 2px con duración de 6 segundos.
  - **Pausa al pasar el mouse:**
    - El temporizador de 6 segundos se pausa cuando `touch-area.has-hover == true`, permitiendo al usuario leerlo con tranquilidad o hacer clic en la acción.
* **Deduplicación y Coalescencia:**
  - Cada notificación incluye una clave de deduplicación opcional: `dedup_key: Option<&'static str>` (ej. `"addon_err_torrentio"`).
  - Si llega un error idéntico mientras el toast ya está visible, se reinicia su temporizador y se incrementa un contador si corresponde, evitando ráfagas de 5 o 6 toasts sucesivos.
* **Desacoplamiento Liviano con Rust:**
  - Se utiliza un enum liviano y serializable para las acciones sin closures:
    ```rust
    #[derive(Clone, Debug)]
    pub enum ToastAction {
        None,
        RetryBoard,
        RetryMetaDetails { meta_id: String },
        RetryStreams { video_id: String },
    }
    ```
  - Un canal seguro `tokio::sync::mpsc` permite invocar `send_toast` desde cualquier hilo (red, motor core, mpv) y despachar al hilo de Slint mediante `slint::invoke_from_event_loop`.

---

## 3. Plan de Fases de Ejecución

1. **Fase 1: Módulo Anti-Bloqueo DoH con Filtro Bogo-IP**
   - Implementar el `reqwest::dns::Resolve` con fallback a DoH Cloudflare/Google para IPs privadas/loopback en `core-env`.
   - Agregar caché `DashMap` para no repetir consultas.
   - Compilar con 4 hilos y validar resolución transparente de `torrentio.strem.fun`.
2. **Fase 2: Watchdog Acotado y Refresco Real de F5**
   - Implementar el Watchdog de 2 intentos con backoff para la cartelera en `board.rs`.
   - Pulir el handler de F5 para que ejecute `Unload` + `LoadRange` sin parpadeo de interfaz.
3. **Fase 3: Componente Slint `ToastSquircle` Flotante y `ToastManager` en Rust**
   - Diseñar `toast.slint` abajo a la derecha con 'X', botón de acción, y pausa por hover.
   - Crear `app/src/toast_manager.rs` con deduplicación por clave y el enum `ToastAction`.
4. **Fase 4: Integración Granular de Errores y Confirmación de Bypass**
   - Notificación única al esquivar bloqueo de ISP por DoH.
   - Fila de alerta compacta en streams si un addon falla sin romper los streams de los demás addons.
