# Roadmap: resiliencia de arranque, red y contenido

**Fecha:** 2026-11-05
**Estado:** Fases 0–6 implementadas entre `0.99.86` y `0.99.88`. La Fase 7 (matriz de pruebas amplia y métricas de 7 días) sigue abierta por depender de uso real.
**Alcance:** recuperación del contenido remoto (catálogos y detalles), arranque en frío, cierre de la aplicación y los dos bugs colaterales confirmados (panics y cierre lento).
**Documentos relacionados:** `Roadmap_Cache_Inteligente_y_Tray.md`, `Roadmap_Cache_Thumbnails_Derivados.md`.

> Regla de este roadmap: primero se diagnostica con evidencia, después se corrige. Nada de "parches a ciegas" sobre el arranque. Cada fase debe poder revertirse sola.

---

## 1. Problema

Tras reiniciar el equipo, a veces la aplicación abre y:

- **Continue Watching carga** (es local), pero las filas de catálogos de addons quedan vacías.
- Al entrar al detalle de una película o serie, **no carga**.
- El contenido **no se recupera solo**: hay que reiniciar la aplicación.

No es un cuelgue de ventana: en los arranques registrados la app alcanza `background application startup completed` en 150–650 ms y la UI responde. Lo que falla es el contenido remoto, y queda en un estado del que no sale.

### Por qué es grave

- El usuario no tiene forma de saber si es su red, el addon o la app.
- El único remedio conocido es cerrar y abrir, que es justo lo que intentamos evitar.
- Es intermitente y depende del momento del arranque, así que no aparece en pruebas manuales repetidas.

---

## 2. Evidencia recogida (confirmada)

### 2.1 Los errores de red se cachean como si fueran datos

En `stremio-core`:

- `models/common/resource_loadable.rs:264` convierte cualquier `Err` en `Loadable::Err(ResourceError::Env(...))`.
- `models/common/resource_loadable.rs:176` reutiliza una entrada si `resource.content.is_some() && !force`.
- `models/catalogs_with_extra.rs:186` hace lo mismo para catálogos: `content.is_some()` incluye `Loadable::Err`.
- `models/meta_details.rs:484` pide los detalles con `force: false`.

Consecuencia: **un solo fallo deja esa fila y ese detalle muertos para toda la sesión**. Reabrir el detalle reutiliza el error.

### 2.2 La carga inicial no espera a la red

- `app/src/callbacks.rs:682-701`: `trigger_initial_load` pide el Board y el rango `0..CATALOG_PRELOAD_ROWS + 1` (`CATALOG_PRELOAD_ROWS = 5`), es decir, las primeras 6 filas.
- `app/src/main.rs:1102`: se llama en cuanto el runtime está construido, sin comprobar conectividad ni esperar al servidor de streaming.

La primera ventana de vulnerabilidad es de aproximadamente un segundo después de abrir.

### 2.3 No existe ruta de recuperación

- `Internal::ProfileChanged` es lo único que reconstruye catálogos, pero **también reutiliza** las entradas con `content.is_some()` (`catalogs_with_extra.rs:150`), así que tampoco limpia los `Err`.
- La app nunca emite `ProfileChanged` directamente.
- La UI sí proyecta el error (`app/src/models/mod.rs:337-341` → `error_message`) y lo muestra en `app/ui/pages/board.slint:166`, pero **no hay botón de reintento**.
- En detalles no hay ni mensaje de error: solo se queda cargando.

### 2.4 El caché HTTP no es el problema principal

Aclaración importante para no repetir un diagnóstico erróneo:

- Las entradas viven hasta **30 días** (`MAX_AGE_SECONDS`), no horas.
- Una entrada 2xx se sirve al instante aunque toque revalidar; la revalidación va en segundo plano (`http_cache.rs:271-279`).
- Las ventanas de "frescura" son 15 min (catálogo con validador), 1 h (meta con validador) y 6 h sin validador.
- La base actual tiene 78 catálogos y 75 metas cacheados, con entradas de hoy.

Por tanto, la teoría "tras reiniciar todo el caché expiró" **no se sostiene**. El problema no es la falta de caché, es que un fallo se graba como estado final.

### 2.5 El disparo del primer fallo sigue sin confirmar

En los journals conservados no hay ni un `Core API request failed` ni un `catalog request failed`. Es decir:

- **Confirmado:** el mecanismo por el que no se recupera.
- **No confirmado:** qué falla exactamente en ese arranque concreto (red a medio levantar, DNS, VPN, primer efecto atascado).

La Fase 0 existe para capturar eso antes de tocar la lógica.

### 2.6 Bugs colaterales confirmados

- **Cierre lento:** `app/src/main.rs:355` arma un watchdog de 6 s; en 5 de los 9 cierres del 2026-11-04 saltó. El proceso tarda en terminar tras cerrar la ventana.
- **Panic al cerrar:** `stremio-core/runtime/runtime.rs:77` hace `try_send(...).expect("emit event failed")`; con `panic = "abort"` produce SIGABRT y el diálogo de crash. Aparece en coredumps del 2026-11-03.
- **Ejecutor único:** `core-env/src/lib.rs:96` procesa efectos en un solo hilo (`handle.block_on`) con cola de 64. Un efecto que no termine atasca todo el trabajo remoto. No hay warnings de saturación en los journals, así que es un riesgo estructural, no la causa observada.
- **Instancia única:** `app/src/single_instance.rs:15` fija el puerto 35984; si está ocupado, `forward` espera hasta 3 s (`:159`) y luego reintenta el bind cada 500 ms (`:60`). Al relanzar mientras la instancia anterior muere, la nueva puede salir sin abrir ventana.

---

## 3. Principios de diseño (las salvaguardas)

Todo lo que se implemente debe cumplir estas reglas. Si una fase las incumple, no se mergea.

1. **Reintento acotado:** máximo 3–5 intentos, backoff exponencial con jitter, techo de 30 s. Nunca reintentos infinitos.
2. **Solo GET idempotentes.** Nada de reintentar escrituras, auth ni reproducción.
3. **No reintentar 4xx**, salvo 408 y 429 (ya es el criterio actual para catálogos).
4. **Un solo intento en vuelo por clave** (singleflight ya existe). Evitar tormentas al recuperar la red.
5. **Recuperación explícita y visible:** botón "Reintentar" por fila y en detalles; mensaje corto, detalle técnico solo en logs.
6. **Recuperación automática con disparadores concretos:** al recuperar conectividad, al volver a enfocar la ventana y al reactivar la pestaña. No en cada frame.
7. **No destruir estado bueno:** al reintentar se recarga solo lo que está en `Err`; el scroll, la pestaña activa y lo ya cargado se conservan.
8. **La UI nunca se bloquea:** reintentos en tareas de Tokio, con semáforo propio.
9. **No envenenar la caché:** los errores de reintento no se escriben en disco; el estado de intentos vive en memoria.
10. **Nada de secretos en logs:** usar `sanitized_url_for_log`, ya existente.
11. **Feature flag** (`STREMIO_STARTUP_RESILIENCE=0`) para desactivar todo el comportamiento nuevo sin recompilar lógica.
12. **Modo offline / ahorro de datos:** si el usuario está sin red o en ahorro, no reintentar en bucle; mostrar el estado.
13. **No interrumpir reproducción** con reintentos de fondo.
14. **Compatibilidad de base de datos:** sin migraciones destructivas; si se añade tabla de diagnóstico, que sea aditiva.
15. **Tests deterministas:** servidor de addons falso que falla a demanda; nada de depender de la red real.
16. **No reescribir tags publicados.** Este trabajo va a una versión nueva.

---

## 4. Fases

### Fase 0 — Observabilidad y captura (implementada en 0.99.86)

**Objetivo:** que el próximo fallo deje evidencia suficiente para no volver a especular.

Trabajo (implementado en `0.99.86`):

- Primer fallo remoto de la sesión registrado con `host`, `resource_kind` y `error`, y repetido en el resumen de métricas (`first_failure_*`).
- Resumen de arranque a los 20 s con filas `ready`, `failed`, `empty` (contenido vacío), `loading`, `idle` y hasta tres mensajes de error distintos.
- `Loadable::Err(EmptyContent)` se cuenta aparte y no dispara ni el aviso ni el botón de reintento.
- Los fallos de red ya se registraban con `tracing::error!` en `send_request`; ahora además queda el primero de la sesión en el resumen.

Criterio de aceptación:

- Reproducir un fallo simulado (cortar red antes de abrir) y que el log permita identificar host, tipo de recurso y momento exacto, sin ambigüedad.

Riesgo: bajo. Solo lectura y logging.

---

### Fase 1 — Recuperación manual sin reiniciar (la más importante) (implementada en 0.99.86)

**Objetivo:** que el usuario pueda arreglarlo con un clic, hoy.

Trabajo:

- Usar `Action::Unload` + recarga, que ya existe en el core y limpia el estado:
  - Board: `catalogs_with_extra.rs:56` vacía `catalogs` y `selected`; después se vuelve a llamar `trigger_initial_load`.
  - Detalles: `meta_details.rs:113` vacía `meta_items`, `streams`, etc.; después se reemite `ActionLoad::MetaDetails` con la misma selección.
- Botón "Reintentar" en la fila con error (`board.slint`, junto al `error-message` actual).
- Botón "Reintentar" en la página de detalles cuando el meta está en `Err`.
- Preservar pestaña, scroll y selección; no recargar filas que ya están `Ready`.
- Debounce y generación: si el usuario pulsa dos veces, solo una recarga.

Nota de diseño: `Unload` es todo-o-nada por modelo. Para preservar filas buenas, la primera versión puede recargar el Board completo (coste aceptable porque todo viene de caché) y en una fase posterior añadir una acción granular al core.

Implementado en `0.99.86`: botón **Retry** en las filas del Board con error (`MetaRow`) y banner **Retry** en Detalles. Ambos hacen `Unload` del modelo afectado y vuelven a lanzar la carga; el Board conserva el resto de filas ya cargadas.

Criterio de aceptación:

- Con la app en el estado roto, pulsar "Reintentar" recupera las filas y los detalles **sin reiniciar**.
- Las filas ya cargadas no parpadean ni pierden el scroll.

Riesgo: medio. Hay que evitar bucles de recarga y dobles peticiones.

---

### Fase 2 — Recuperación automática (reintento programado implementado en 0.99.87)

**Objetivo:** que en la mayoría de los casos el usuario ni lo note.

Implementado en `0.99.87`: reintento programado de filas del Board en error, 3 intentos con backoff 5 s, 15 s y 45 s, sin contar `EmptyContent`, desactivable con `STREMIO_STARTUP_RESILIENCE=0`. El presupuesto se reinicia cuando el Board vuelve a estar sano. Pendientes: disparadores por conectividad D-Bus, foco de ventana y pestaña reactivada.

Disparadores:

1. **Conectividad recuperada.** Escuchar `org.freedesktop.NetworkManager` (`Connectivity`/`State`) por D-Bus; si hay red, reintentar solo las secciones en `Err`.
2. **Ventana enfocada** tras haber estado sin foco, con un mínimo de 5 s entre intentos.
3. **Pestaña reactivada**, si esa pestaña tiene errores.
4. **Reintento programado:** 3 intentos con backoff 1 s, 4 s, 15 s, con jitter.

Salvaguardas específicas:

- Contador por sección; tras agotar intentos, dejar de reintentar hasta un disparador manual o de conectividad.
- No reintentar si la app está reproduciendo o si el usuario marcó modo offline.
- Semáforo global (máximo 2–4 recargas concurrentes) para no saturar.
- Si el error es 404 o contenido vacío, no reintentar: no es transitorio (ver Fase 4).

Criterio de aceptación:

- Escenario "red caída al abrir, red restaurada 10 s después": las filas aparecen solas en menos de 5 s tras restaurar la red, sin reiniciar.
- Escenario "red ausente toda la sesión": como máximo 3 intentos por sección y cero bucles.

Riesgo: medio-alto por el watcher D-Bus. Aislarlo tras el flag de la Fase 0.

---

### Fase 3 — Preparar el arranque sin bloquear la UI (implementada en 0.99.88)

**Objetivo:** reducir la probabilidad del fallo inicial, sin retrasar la primera pintura.

Trabajo:

- No bloquear la ventana esperando red. En su lugar:
  - Arrancar con la carga inicial optimista actual.
  - Si a los 2 s no hay ninguna respuesta de red correcta, marcar el estado como "sin conexión" y no grabar `Err` permanente.
- Comprobar conectividad de forma barata y acotada (timeout 1,5 s) antes de la primera tanda de catálogos; si no hay red, dejar las filas en `Loading` con mensaje "Sin conexión" en vez de `Err` cacheado.
- Reordenar: primero las filas locales (Continue Watching, Library) y después las remotas, para que el arranque siempre muestre algo útil.

Criterio de aceptación:

- Arranque con red desactivada: la UI aparece igual de rápido, Continue Watching visible, filas remotas con mensaje claro y recuperables.
- Arranque normal: sin regresión medible en `shell_ready_ms` (hoy 150–650 ms).

Riesgo: bajo-medio. El riesgo es añadir latencia; se mide.

---

### Fase 4 — Endurecer el caché HTTP (completa en 0.99.87–0.99.88)

**Objetivo:** que la red no sea imprescindible para lo que ya se vio.

Implementado en `0.99.87`: una entrada 2xx válida se conserva como fallback aunque supere la ventana de retención de 30 días; si la red falla, se sirve y sube `stale_served_on_error`. En `0.99.88`: métricas de stale por tipo de recurso y botón **Clear Addon Cache** en Ajustes → Backup & Diagnostics.

Trabajo:

- Servir la entrada válida (hasta 30 días) ante fallo de red **siempre**, no solo cuando el estado no es 2xx. Hoy el fallback existe pero está limitado a `stale_entry`, que solo se llena en una rama concreta (`http_cache.rs:268-289`). Unificar el criterio: si hay cuerpo válido y la red falla, servir y marcar `stale_served_on_error`.
- Métrica separada para "servido de caché por fallo de red" por tipo de recurso.
- No cachear como éxito respuestas vacías: `resource_vector_content_from_result` convierte `content.is_empty()` en `Err(EmptyContent)`; decidir si un catálogo legítimamente vacío debe ser `Ready(vec![])` (requiere parche del core, Fase 5).
- Invalidación manual: botón "Vaciar caché de addons" en Ajustes → Diagnóstico, sin tocar imágenes ni ajustes.

Criterio de aceptación:

- Abrir la app sin red, con caché de una sesión anterior: filas y detalles antes vistos se muestran.
- El contador de `stale_served_on_error` sube y el de `network_error` no genera errores visibles.

Riesgo: bajo. Es una mejora localizada en `core-env/src/http_cache.rs`.

---

### Fase 5 — Core: semántica de errores reintentables (implementada en 0.99.88 con parche vendorizado)

**Objetivo:** arreglar la raíz, no solo la app.

Opciones, de menor a mayor intrusión:

1. **PR upstream** a `stremio-core`: tratar `Loadable::Err` como no cacheado cuando se vuelve a pedir la misma acción, o añadir `force` a `MetaDetails`/`CatalogsWithExtra`.
2. **Parche vendorizado** si upstream no responde: mantener un fork mínimo con esas dos líneas y documentarlo en `PATCHES`, igual que se hace con `i-slint-core`.
3. No parchear y quedarse con la solución de Fase 1/2.

Recomendación: intentar (1); si no hay respuesta en un ciclo de release, (2) con un diff mínimo y tests propios.

Implementado en `0.99.88` con la opción (2): `vendor/stremio-core` (1,9 MB, sin historial) aplica dos cambios documentados en `PATCHES.md`: no reutilizar `Loadable::Err` al repetir una petición y aceptar catálogos vacíos como `Ready(vec![])`. El `Cargo.toml` lo activa con `[patch."https://github.com/perpetus/stremio-core.git"]`.

Criterio de aceptación:

- Un fallo transitorio no impide volver a cargar la misma fila/detalle en la misma sesión, aun sin `Unload`.

Riesgo: alto (dependencia externa). Por eso va después de las fases que ya resuelven el problema.

---

### Fase 6 — Bugs colaterales

#### 6.1 Panic `emit event failed` (prioridad alta: es un crash) — implementado en 0.99.86

- `stremio-core/runtime/runtime.rs:77`: `try_send(...).expect(...)`.
- Opciones: parche vendorizado que convierta el `expect` en `let _ =` (perder el evento al cerrar es correcto), o garantizar que no se despache después de cerrar el runtime.
- Test: cerrar la app 50 veces con reproducción y sin ella; cero SIGABRT.

#### 6.2 Cierre lento (6 s) — implementado en 0.99.88

- `app/src/main.rs:355`: watchdog de 6 s.
- Investigar qué bloquea después de los logs de métricas (probablemente el drop del runtime de Tokio esperando tareas bloqueantes).
- Medidas:
  - Traza de tiempos por etapa de `AppSession::shutdown`.
  - Bajar el watchdog a 3 s solo si las métricas demuestran que el cierre normal es <1 s.
  - Cerrar explícitamente los hilos bloqueantes (librqbit, mpv) antes del drop.
- Criterio: p95 de cierre <1 s; watchdog solo como último recurso.

#### 6.3 Instancia única — implementado en 0.99.88

- Añadir `SO_REUSEADDR` y un log cuando el bind falle.
- Acortar la espera de `forward` a 1,5 s y no reintentar el bind si el puerto está ocupado por un proceso vivo.
- Criterio: relanzar mientras la instancia anterior cierra no deja la app "sin abrir".

#### 6.4 Tray en frío — medición implementada en 0.99.88

- `app/src/tray/standard.rs:14` y `vendor/i-slint-core/items/system_tray/ksni.rs:300` crean el tray de forma bloqueante en el hilo de UI.
- Medir el tiempo de `tray::setup` y, si supera 250 ms, registrar warning. Si se confirma bloqueo en frío, diferir la creación 2 s después del primer frame o comprobar antes que exista `StatusNotifierWatcher`.

---

### Fase 7 — Validación, métricas y release

#### Matriz de pruebas

| Escenario | Cómo se simula | Resultado esperado |
|---|---|---|
| Arranque sin red | `nmcli networking off` antes de abrir | UI rápida, filas con "Sin conexión", sin `Err` permanente |
| Red restaurada | Activar red a los 10 s | Filas cargan solas < 5 s |
| Fallo intermitente | Servidor de addons falso que falla 2 de 3 | Reintento transparente, sin filas muertas |
| DNS lento | `tc netem delay` o resolver falso | Sin `Err` permanente; timeout acotado |
| Detalle con error | Cortar red y abrir detalle | Mensaje + "Reintentar"; recupera al volver la red |
| Relanzar durante cierre | Cerrar y abrir en < 2 s | La nueva instancia abre o espera, nunca falla en silencio |
| Cierre | 50 cierres seguidos | p95 < 1 s, cero SIGABRT |
| Caché | Segunda apertura sin red | Filas y detalles antes vistos se muestran |

#### Métricas de aceptación

- `stuck_sections_30s = 0` en el 99 % de arranques con red.
- Recuperación tras restaurar red: < 5 s, sin reinicio.
- Peticiones extra por reintentos: < 20 % sobre el total en sesión normal.
- Cero `emit event failed` durante 7 días de uso.
- Cierre p95 < 1 s.
- `shell_ready_ms` sin regresión (≤ 650 ms en el equipo de referencia).

#### Plan de release

- Versión nueva (`0.99.86`), sin mover `v0.99.85`.
- CHANGELOG con: reintento manual, reintento automático, caché de respaldo y fixes colaterales.
- Orden: Fase 0 + 1 + 6.1 (crash) → 0.99.86; Fases 2–4 → 0.99.87; Fase 5 → cuando upstream/parche esté listo.

---

## 5. Orden recomendado

1. **Fase 0** — capturar evidencia (1 sesión).
2. **Fase 1** — botón Reintentar con `Unload` + recarga (1–2 sesiones).
3. **Fase 6.1** — eliminar el crash al cerrar (1 sesión).
4. **Fase 2** — reintento automático + conectividad (2 sesiones).
5. **Fase 3** — arranque sin bloquear y estado "sin conexión" (1 sesión).
6. **Fase 4** — fallback de caché (1 sesión).
7. **Fase 6.2–6.4** — cierre, instancia única y tray (1–2 sesiones).
8. **Fase 5** — core, solo si hace falta.

## 6. Riesgos y mitigaciones

| Riesgo | Mitigación |
|---|---|
| Bucles de reintento | Contador + backoff + flag + tests con servidor falso |
| Recargar el Board pierde scroll | Recargar solo en `Err`; conservar estado en la proyección |
| El watcher D-Bus falla en otros escritorios | Fallback a reintento por foco; nunca dependencia dura |
| Latencia añadida al arranque | Todo asíncrono; medir `shell_ready_ms` en cada PR |
| Parche del core difícil de mantener | Fase 5 al final; diff mínimo y documentado |
| Ruido en logs | Rate limit por host y por sesión |

## 7. Fuera de alcance

- Cambios de reproducción, MPV o códecs.
- Rediseño visual del Board o de Detalles.
- Sincronización de perfiles o cambios de autenticación.
- Litecord y cualquier otro repositorio.

## 8. Definición de terminado

- Un fallo transitorio de red **nunca** deja contenido muerto hasta reiniciar.
- El usuario ve un mensaje claro y puede reintentar.
- La app se recupera sola al volver la red en la mayoría de los casos.
- Cerrar no tarda más de 1 s ni genera crash reports.
- Todo cubierto por tests y con métricas que lo demuestren.

---

## 9. Cuelgue intermitente del runtime (en curso)

Síntoma confirmado en la sesión de `2026-11-06T01:42Z`: 7 filas quedaron en `Loading`, el watchdog las reintentó dos veces y a las `01:43:25Z` el heartbeat registró `async runtime appears stalled`. Los 21 workers de tokio estaban en `futex_wait` y la app ya no volvió a responder hasta cerrarse a mano.

### Fase 7.1 — Diagnóstico (implementada en 0.99.88)

- [x] Heartbeat de tokio (tarea + hilo del sistema) que detecta bloqueos de 5 s y vuelca el estado de todos los hilos.
- [x] El volcado se repite cada 30 s mientras el bloqueo persiste y se registra la duración al recuperarse.
- [x] `Runtime::model()` registra el hilo y el sitio (`file:line`) que tiene el guard de lectura.
- [x] Toda adquisición de lectura/escritura del modelo sondea 2 s con `try_read`/`try_write`; si no la obtiene, registra quién la tiene, si el que espera ya tenía una lectura (lectura reentrante = deadlock) y vuelca los hilos.
- [x] El comportamiento del lock no cambia: solo se añade visibilidad.
- [x] El volcado del heartbeat incluye permisos disponibles y conexiones ociosas del pool de base de datos; una conexión retenida más de 10 s avisa al devolverse.
- [x] Corregido un panic del executor secuencial (`there is no reactor running`) que cerraba la app al primer efecto secuencial en release.

### Fase 7.2 — Corrección (pendiente)

- [ ] Usar el informe del próximo cuelgue para identificar el guard que nunca se libera.
- [ ] Según el resultado: acortar guards, prohibir lecturas anidadas o mover las escrituras del modelo a un hilo dedicado.
- [ ] Añadir una red de seguridad en la UI: fila en error con reintento tras 60 s y aviso si el runtime lleva >30 s caído.

### Fase 7.3 — Pruebas (pendiente)

- [ ] Test determinista de contención: un lector mantiene el guard y un escritor debe registrar el aviso y completar al soltarse.
- [ ] Bucle de ~20 arranques buscando `model lock waited too long` o `runtime appears stalled`.
