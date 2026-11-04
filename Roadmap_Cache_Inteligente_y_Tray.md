# Investigación y roadmap: caché inteligente y restauración de ventana desde el tray

**Fecha de investigación:** 2026-11-04
**Revisión del plan:** 2026-11-04, ampliado con diseño de implementación, orden de ejecución y pruebas.
**Base:** working tree sobre `v0.99.7fix` (`c0b1aa2`) con el toggle de tray y el WIP de conexiones Turso sin commitear.
**Alcance:** investigación y especificación. Este documento no representa trabajo de implementación ya realizado.

---

## 1. Resumen ejecutivo

Hay dos problemas independientes:

1. **Tray en Wayland:** la ventana no se restaura desde minimizada y pierde tamaño/maximizado al mostrarse desde el tray. La causa es la combinación de las limitaciones de winit/Wayland con la destrucción y recreación de la superficie que realiza Slint. Se aplica el estado demasiado pronto y se intenta usar APIs que en Wayland son no-op.
2. **Sensación de “se recarga todo”:** ya existen cachés HTTP y de imágenes. El problema principal no es su ausencia, sino la coordinación entre SQLite, red, revalidación, modelo de UI y decodificación:
   - el antiguo timeout de 2 s convertía aciertos de caché en peticiones de red;
   - una única conexión Turso serializaba caché, perfil y otras escrituras;
   - todos los recursos revalidan a los 60 s y las revalidaciones se disparan en bloque;
   - los streams no se reutilizan;
   - todavía no hay métricas suficientes para saber cuánto de la recarga visual corresponde a red y cuánto a reconstruir modelos o decodificar imágenes.

La estrategia correcta es **cerrar y medir el WIP, instrumentar, corregir la política de revalidación y el tray, y sólo después valorar cambios invasivos en catálogos o imágenes**. No se debe empezar por un diff complejo ni por persistir bitmaps RGBA sin evidencia de que sean necesarios.

---

## 2. Tray: evidencia y causa raíz

### 2.1 Comportamiento observado

Prueba en **KDE Plasma Wayland (KWin 6.8.2)**, con una copia de perfil real y el binario release:

- Ventana maximizada (`maximizeMode = 3`, geometría 1920×1080 lógicos).
- Clic izquierdo en el tray → se oculta.
- Clic izquierdo de nuevo → reaparece a **1280×819 y sin maximizar** (`maximizeMode = 0`), aunque la configuración guardada decía `window_maximized = true`.
- Con la ventana minimizada (`minimized = true`) y luego clic en el tray: la ventana sigue minimizada (`minimized = true`, `active = false`). El log confirma: `Unminimizing is ignored on Wayland.`

En X11, forzando X11 al quitar `WAYLAND_DISPLAY`, el mismo flujo sí restaura desde minimizado y conserva maximizado. El defecto principal es específico del camino Wayland.

### 2.2 Por qué ocurre

En winit 0.30/Wayland:

- `is_minimized()` devuelve `None`;
- `set_minimized(false)` se ignora;
- `focus_window()` es no-op;
- `set_visible()` es no-op;
- Slint destruye la superficie al ocultar y la recrea al mostrar;
- al recrear, Slint no garantiza reaplicar el tamaño preferido/maximizado al nuevo `xdg_toplevel`.

El orden actual de `app/src/tray.rs::show_window()` aplica tamaño/maximizado antes de `ui.show()` y después intenta desminimizar/enfocar. En Wayland la nueva ventana winit puede todavía no existir, y el retry actual sólo repite el foco, que también es no-op.

### 2.3 Diseño de implementación del tray

#### T1. Centralizar las transiciones

Crear una única entrada de restauración en `app/src/tray.rs`, utilizada por:

- toggle del clic izquierdo;
- “Open Stremio Oxide”;
- apertura de settings;
- avisos de actualizaciones;
- cualquier otra acción que traiga la ventana desde el tray.

La regla debe ser:

- **toggle:** si está visible y enfocada, ocultar; en cualquier otro caso, restaurar;
- **Open/settings/updates:** restaurar siempre, nunca ocultar;
- **PiP activo:** no ocultar el video; restaurar la ventana completa según el comportamiento ya establecido.

No duplicar secuencias `show/set_size/focus` en varios callbacks.

#### T2. Capturar estado antes de ocultar

Mantener en memoria, en el hilo de UI, una estructura equivalente a:

- tamaño lógico;
- estado maximizado;
- estado fullscreen;
- posición sólo si se decide restaurarla de forma portable;
- generación/identificador de la captura.

Al ocultar:

1. leer el estado real mediante `with_winit_window` cuando esté disponible;
2. guardarlo en memoria antes de llamar a `hide()`;
3. conservar `app_config` como fallback para un reinicio, no como única fuente durante hide/show;
4. no sobrescribir un tamaño válido con el tamaño temporal de una ventana minimizada, fullscreen o ya oculta.

`persist_window_geometry` puede seguir encargándose de SQLite, pero la restauración inmediata no debe depender de que su debounce haya terminado.

#### T3. Añadir una pequeña máquina de estados

Usar estados conceptuales `Visible`, `Hidden` y `Restoring`, más un contador de generación:

- cada restauración incrementa la generación;
- cada retry conserva esa generación y abandona si ya es antigua;
- un nuevo clic durante `Restoring` no debe lanzar otra cadena de timers;
- al terminar, pasar a `Visible` o `Hidden` según el resultado real conocido.

La implementación debe vivir en el hilo de UI (`RefCell`/estado local o equivalente), no introducir un mutex global. Los callbacks diferidos deben usar `Weak<MainWindow>` y comprobar que la ventana sigue existiendo.

#### T4. Restaurar después de que exista el surface

Secuencia normal:

1. capturar el estado objetivo;
2. llamar a `ui.show()`;
3. intentar `with_winit_window`;
4. si todavía no hay ventana winit, reintentar con un `slint::Timer` single-shot en intervalos cortos, por ejemplo 16, 50, 150 y 400 ms;
5. cuando exista, aplicar en winit:
   - salir de minimizado sólo en plataformas donde tenga efecto;
   - reaplicar maximizado;
   - si no estaba maximizada, reaplicar tamaño;
   - restaurar fullscreen si procede;
   - solicitar foco/activación en plataformas que lo soporten;
6. no seguir reaplicando tamaño indefinidamente.

Antes de un retry tardío, comprobar que el usuario no haya redimensionado manualmente. Si el tamaño actual difiere del tamaño inicial observado tras el show y no fue cambiado por nuestra propia restauración, cancelar los retries de tamaño.

No usar sleeps bloqueantes ni mantener referencias fuertes a la UI dentro de tareas asíncronas.

#### T5. Remapeo de contingencia en Wayland

En Wayland no se puede consultar de forma fiable `minimized`. Para una acción explícita de restaurar cuando la ventana no está visible/enfocada, el camino portable disponible es recrear el surface:

1. no hacer `hide()+show()` si la ventana está visible y enfocada;
2. cuando el tray solicita restaurar y el estado visible/focus indica que no está al frente, capturar estado;
3. ejecutar `hide()` y después `show()` una sola vez;
4. aplicar T4 sobre el nuevo surface;
5. no repetir el remapeo en bucle si el compositor no concede foco.

Este remapeo es una contingencia para Wayland, no el comportamiento normal de Windows/X11. Debe medirse su efecto sobre MPV y previews. No se añadirá integración específica con KWin D-Bus como solución principal.

#### T6. Geometría, DPI y monitores

- Usar tamaño lógico de Slint al cruzar la frontera Slint/winit, evitando mezclar píxeles físicos y lógicos.
- En Windows comprobar escalados 100/125/150/200 %.
- Si se restaura posición, verificar que intersecta un monitor conectado; si no, omitir posición y dejar que el compositor la coloque.
- No intentar fijar posición en Wayland, donde el cliente no la controla de forma general.

#### T7. Pruebas obligatorias del tray

Automatizar donde sea posible:

- visible enfocada → clic → oculta;
- oculta → clic → visible con mismo tamaño;
- maximizada → hide/show → sigue maximizada;
- minimizada → clic → restaurada;
- ventana tapada → clic → se intenta traer al frente;
- cinco clics espaciados → cinco transiciones, sin estados invertidos;
- doble clic rápido → sin cadenas de restauración concurrentes;
- PiP → no desaparece el video;
- fullscreen → vuelve a fullscreen si ese estado estaba activo.

Validar con `xdotool` en X11 y con scripts de KWin en Wayland (`frameGeometry`, `maximizeMode`, `minimized`, `active`). Windows requiere validación manual adicional. La prueba debe comprobar estado real del compositor, no sólo mensajes de log.

---

## 3. Caché: inventario actual

### 3.1 Caché HTTP (`core-env/src/http_cache.rs`)

- Cachea `GET` sin `Authorization` ni `Cookie` para `manifest`, `catalog`, `addon_catalog`, `meta`, `legacy_catalog` y `legacy_meta`.
- Clave: hash de `GET + URL canónica + Accept + Accept-Language`.
- Guarda cuerpo, `ETag`, `Last-Modified`, timestamps y tamaño.
- Entrada máxima 8 MB, total 256 MB, retención máxima 30 días.
- Usa stale-while-revalidate y peticiones condicionales.
- Un `304` actualiza validación sin reescribir el cuerpo.
- `REVALIDATE_AFTER_SECONDS = 60` para todos los recursos.
- Single-flight sólo para revalidaciones, no para misses simultáneos.
- No cachea `/stream/...`.

### 3.2 Imágenes (`app/src/image_cache.rs`)

- Memoria: 48 MB, idle TTL 5 min, TinyLFU.
- Disco: 384 MB, 30 días, hash BLAKE3, sharding y escritura atómica.
- URLs TMDB/Metahub optimizadas antes de hashear.
- Descargas limitadas, workers y cola LIFO para priorizar lo visible.
- No persiste bitmap decodificado: cada arranque vuelve a leer y decodificar.

### 3.3 Modelo/UI

- Board carga filas visibles con `LoadRange` y precarga cinco filas alrededor.
- Al cambiar Board/perfil se reconstruyen secciones y se resuelven imágenes por URL.
- Library/progreso/Continue Watching viven en almacenamiento local.
- Búsqueda y filtros se cachean como catálogos por URL.

---

## 4. Diagnóstico que debe guiar la implementación

Prioridad de causas:

1. timeout de caché que convertía hits en misses;
2. contención de una conexión Turso única;
3. revalidación uniforme y simultánea;
4. streams sin reutilización;
5. posible reconstrucción del modelo y redecodificación de imágenes.

No asumir que todo parpadeo es red. La instrumentación debe distinguir al menos:

- espera de SQLite;
- espera de red;
- parseo/mapeo del modelo;
- reemplazo de filas en Slint;
- lectura y decodificación de imágenes.

Un hit de caché debe mostrar contenido sin esperar a la revalidación. Un `304` no debe reconstruir la UI ni reescribir el cuerpo. Un cambio real de catálogo no debe borrar primero la fila y dejar un skeleton si ya existe contenido válido.

---

## 5. Matriz de escenarios y comportamiento requerido

| Escenario | Comportamiento requerido |
|---|---|
| Catálogo sin cambios | Servir caché inmediatamente; revalidar sólo si venció su política; `304` no toca la fila |
| Nueva película | Aceptar la nueva respuesta; si se implementa diff, insertar/mover sólo lo necesario |
| Reordenamiento | Reflejar el orden nuevo conservando objetos no modificados |
| Nuevo episodio | Revalidar meta según su política; no recargar catálogos no relacionados |
| Error de red/addon | Servir stale disponible dentro de la retención y aplicar backoff |
| Respuesta `200` válida con `metas: []` | Aceptar el catálogo vacío; no confundirlo con un error |
| Respuesta malformada | Conservar stale y registrar el error sin URL sensible |
| Detalles ya visitados | Meta desde caché; streams desde memoria sólo si la entrada sigue vigente |
| Búsqueda repetida | Reutilizar respuesta y unir peticiones idénticas concurrentes |
| Scroll rápido | Priorizar viewport; no acumular downloads/revalidaciones obsoletas |
| Cambio de perfil/addons | Aislar o invalidar entradas cuya identidad haya cambiado |
| Segundo arranque | Proceso frío, cachés calientes: contenido antes que revalidaciones background |

---

## 6. Roadmap de implementación

### Fase 0 — Validar el WIP antes de ampliar el diseño

**Estado:** el build release terminó correctamente y contiene `slow HTTP cache read`. Falta validación runtime.

#### Implementación/pasos

1. Limpiar únicamente los warnings directamente introducidos por el WIP, sin refactor adicional.
2. Confirmar que no existe ninguna instancia previa usando el puerto global de single-instance.
3. Ejecutar el release con una copia del perfil, nunca borrando el caché real del usuario.
4. Hacer primero 5 smoke tests; después 20–30 ejecuciones automatizadas si el resultado es estable.
5. Registrar por ejecución:
   - antiguos `HTTP cache read timed out`;
   - `slow HTTP cache read` y duración;
   - tiempo hasta primera fila con contenido;
   - tiempo hasta filas precargadas estables;
   - crashes, skeletons persistentes y errores de base de datos.
6. Comparar pool de 2, 4 y 8 conexiones si persisten lecturas lentas. No asumir que 8 es óptimo: las escrituras SQLite siguen serializándose.
7. Mantener fuera del lock de DB cualquier acceso al llavero y verificar que no haya otro I/O lento dentro de `get_db_conn` o del lock de perfil.

#### Criterio para avanzar

- Cero antiguos timeouts de 2 s.
- Sin errores de locking ni regresiones de perfil.
- Las lecturas lentas restantes quedan identificadas con duración y contexto.
- Si sigue habiendo bloqueos, perfilar antes de cambiar TTLs.

---

### Fase 1 — Instrumentación mínima y benchmark repetible

Implementar contadores en `core-env` y reutilizar/ampliar los contadores existentes de imágenes en `app`.

#### Métricas HTTP

Agrupar por `ResourceKind`, sin incluir URLs completas:

- `cache_hit_fresh`;
- `cache_hit_stale`;
- `cache_miss`;
- `revalidate_scheduled`;
- `revalidate_304`;
- `revalidate_200`;
- `network_error`;
- `stale_served_on_error`;
- `singleflight_joined`;
- `response_bytes`;
- histogramas o agregados de `db_read_ms`, `network_ms` y `db_write_ms`.

#### Métricas de imágenes/UI

- memory hit, disk hit, download y failure;
- tiempo de lectura de disco;
- tiempo de decodificación;
- número de imágenes decodificadas;
- filas reemplazadas y filas actualizadas parcialmente;
- tiempo hasta primer contenido y hasta contenido estable.

#### Cómo exponerlas

- Emitir un resumen agregado tras la carga inicial, por ejemplo a los 15 s, y otro al cierre limpio.
- No emitir un log por póster ni por cada lectura SQLite en funcionamiento normal.
- Redactar query strings y nunca registrar tokens, cuerpos de streams o credenciales.
- Guardar resultados del benchmark fuera del árbol o en artefactos ignorados; no añadir datos de perfil al repositorio.

#### Benchmark

Usar siempre el mismo perfil y definir correctamente el escenario: **proceso recién iniciado con caché HTTP e imágenes calientes**. Añadir escenarios separados: caché vacía, red desconectada y addon con error.

**Done:** se puede afirmar cuántas descargas, revalidaciones, escrituras y decodificaciones ocurren en cada arranque y comparar dos builds sin interpretar visualmente la UI.

---

### Fase 2 — Política de revalidación, scheduler y consistencia

Esta es la primera fase de comportamiento nueva de caché.

#### 2.1 Separar política de la lectura/escritura

Introducir una política pequeña por `ResourceKind`, sin dispersar constantes por el código. Valores iniciales orientativos:

- manifest/addon catalog: 6–24 h;
- catálogo con validador: 15–30 min;
- catálogo sin validador: hasta 6 h;
- meta: 1–6 h;
- búsqueda: TTL más corto si se puede distinguir de forma fiable;
- streams: no entran en esta caché persistente; se tratan en Fase 4.

Una entrada dentro de su intervalo fresco se devuelve sin crear ninguna tarea de red. La revalidación nunca debe retrasar la respuesta cacheada.

Respetar señales HTTP útiles cuando existan, pero no asumir que los addons implementan `Cache-Control` correctamente.

#### 2.2 Scheduler background acotado

Crear una cola en memoria para revalidaciones:

- máximo inicial de 2 revalidaciones concurrentes;
- prioridad para recursos visibles;
- jitter pequeño para evitar ráfagas;
- deduplicación por `cache_key`;
- descarte de trabajo obsoleto al cambiar de perfil;
- los misses de contenido visible no deben esperar detrás de una cola de revalidaciones.

No es necesario persistir la cola. Al arrancar, la primera fila y los requests de foreground tienen prioridad absoluta.

#### 2.3 Single-flight para misses y revalidaciones

Mantener un mapa en memoria `cache_key -> operación compartida`:

- el primer request ejecuta red/DB;
- los siguientes esperan el mismo resultado;
- se elimina la entrada al finalizar, también en error/cancelación;
- una revalidación background no debe convertir la respuesta foreground en bloqueante.

Añadir pruebas con N peticiones concurrentes idénticas y comprobar que sólo sale una petición de red.

#### 2.4 Evitar carreras de escritura

Asociar una generación por clave a las operaciones de red. Antes de guardar:

1. comprobar que la operación sigue siendo la más reciente para esa clave;
2. si otra operación posterior ya guardó resultado, descartar la antigua;
3. no depender únicamente de timestamps de reloj;
4. aplicar lo mismo a `200`, `304` y errores que actualicen metadatos.

Esto evita que una respuesta lenta de un filtro/perfil anterior sobrescriba contenido nuevo.

#### 2.5 Stale-on-error y backoff

- En timeout, error de conexión, `5xx` o respuesta inválida, devolver stale si existe dentro de la retención física.
- No servir silenciosamente stale para un `200` válido, aunque venga vacío.
- Mantener backoff por host en memoria: base corto, exponencial, tope y jitter.
- No reintentar en cada render mientras el host esté en backoff.
- Un `404` u otro `4xx` definitivo no debe provocar reintentos agresivos.

#### 2.6 Revalidación adaptativa, después de estabilizar lo anterior

Como mejora pequeña y opcional dentro de esta fase:

- varios `304` consecutivos aumentan gradualmente el intervalo hasta el máximo del recurso;
- un `200` con cambios reduce el intervalo dentro de límites;
- errores activan backoff, no un refresh más frecuente.

Guardar este estado en memoria inicialmente. No añadir migración SQLite salvo que se demuestre necesario.

#### Pruebas

- fresh hit no genera red;
- stale hit responde antes que la revalidación;
- `304` conserva bytes y no cambia revisión de UI;
- `200` válido vacío reemplaza el contenido;
- error conserva stale;
- una respuesta antigua no sobrescribe una nueva;
- ráfaga de catálogos no supera la concurrencia configurada;
- cambio de perfil no reutiliza trabajos incompatibles.

---

### Fase 7 — Tray, en paralelo a Fases 1–2

Implementar exactamente T1–T7 de la sección 2.3. Es un cambio independiente y debe mantenerse en un alcance separado del caché.

**Orden recomendado:** captura de estado y restauración común → retries del surface → máquina de estados → contingencia Wayland → pruebas X11/Wayland → validación Windows.

No mezclar en este cambio mejoras de caché, geometría no relacionada ni actualizaciones de dependencias Slint/winit.

---

### Fase 4 — Caché de streams en memoria

Se adelanta respecto al diff de catálogos porque es menos invasiva y ataca una latencia directamente visible.

#### Diseño

- Cachear sólo en memoria, con TTL inicial de 2 min y límite acotado de entradas/bytes.
- Usar una clave derivada de la petición completa: URL canónica, addons/configuración, headers que cambien la representación y contexto de perfil. Guardar únicamente un hash, no credenciales en texto claro.
- El ID completo de Stremio debe conservar temporada/episodio cuando formen parte de la petición.
- Cachear sólo respuestas exitosas y válidas; no cachear errores de larga duración.
- Single-flight por clave.
- Vaciar/invalidar al cambiar perfil o configuración de addons.
- Nunca escribir cuerpos de streams en SQLite, archivos de volcado o logs.

#### Prefetch

- No hacer prefetch de streams al hover.
- Prefetch de meta opcional tras 300–500 ms de hover estable, cancelable y deduplicado.
- Los streams se solicitan al abrir detalles o mediante una acción explícita.
- Añadir “actualizar enlaces” para omitir/invalidar la entrada de sesión.

#### Pruebas

- dos aperturas del mismo episodio dentro del TTL producen una petición;
- episodios distintos no comparten entrada;
- cambio de perfil/addon invalida;
- expiración vuelve a pedir;
- URLs firmadas nunca aparecen en logs;
- cancelar detalles no deja tareas descontroladas.

---

### Fase 3 — Diff incremental de catálogos, condicionado a evidencia

Implementar sólo si las métricas muestran parpadeo o coste significativo al reemplazar filas que ya tenían contenido.

#### Ubicación

Hacerlo primero en la capa de adaptación del modelo de UI (`app`), no modificando `stremio-core` salvo imposibilidad demostrada.

#### Identidad y diff

La identidad debe incluir al menos addon, catálogo, tipo, página e ID del item. No usar sólo `item.id`.

Al recibir una página válida:

1. construir el orden nuevo completo;
2. reutilizar objetos existentes sólo si identidad y metadatos relevantes coinciden;
3. insertar elementos nuevos;
4. mover los reordenados;
5. actualizar los modificados;
6. eliminar los que desaparecieron;
7. conservar selección/scroll si el item seleccionado sigue presente.

No realizar un merge que deje items eliminados mezclados con la respuesta nueva. El objetivo es que el resultado final sea exactamente la respuesta válida, pero con menos mutaciones de Slint.

#### Reglas de frescura

- Un `304` no ejecuta diff.
- Un `200` idéntico no debe tocar el modelo.
- Un `200` vacío válido sí vacía la página.
- Error/JSON inválido conserva lo anterior.
- Refrescar página 0 en foreground; páginas restantes bajo demanda.

#### Pruebas

Snapshots con: respuesta idéntica, una inserción, un borrado, reordenamiento, cambio de póster, cambio de metadatos, página vacía, respuesta inválida y duplicados. Verificar contenido final y número de mutaciones, no sólo que “se ve bien”.

---

### Fase 5 — Imágenes, primero reducir trabajo y sólo después cambiar formato

No comenzar guardando RGBA en disco.

#### Orden de implementación

1. Medir lectura, decodificación y subida/render por separado.
2. Decodificar únicamente imágenes visibles y una margen pequeña de prefetch.
3. Redimensionar durante la decodificación cuando el decoder lo permita; evitar decodificar un póster enorme para una tarjeta pequeña.
4. Limitar workers de decodificación para no saturar CPU ni bloquear UI.
5. Mantener la clave por URL optimizada y comprobar que URLs equivalentes no generen entradas duplicadas.
6. Priorizar/cancelar trabajo que salió del viewport antes de empezar; no cancelar writes atómicos a mitad de forma insegura.
7. Separar por rol/tamaño sólo si realmente se solicitan variantes distintas.

Sólo si los datos muestran que la decodificación sigue dominando el arranque, evaluar un formato intermedio. Antes de adoptarlo documentar: tamaño máximo, consumo de disco, versión de formato, invalidación y coste de escritura. RGBA comprimido puede aumentar mucho I/O y no se dará por aprobado implícitamente.

**Done:** segundo arranque sin descargas repetidas y con decodificación limitada al viewport, con mejora medida de tiempo a contenido estable.

---

### Fase 6 — Persistencia de vista, opcional y al final

Sólo abordarla si el usuario necesita volver exactamente a la vista anterior. No usarla como parche para ocultar cargas lentas.

- Persistir pestaña, filtros, catálogo seleccionado y posición lógica por item, no sólo índice.
- Versionar el formato y validar datos al leer.
- Rehidratar antes de solicitar rangos visibles.
- Si el catálogo cambió, localizar el item por identidad; si ya no existe, usar fallback seguro.
- No impedir una carga inicial necesaria por confiar en estado obsoleto.
- Mantener separación por perfil.

Pruebas: reinicio, catálogo reordenado, item eliminado, perfil distinto y configuración corrupta.

---

## 7. Orden ejecutivo recomendado

1. **Fase 0:** validar el release/WIP.
2. **Fase 1:** instrumentación mínima y baseline.
3. **Fase 2:** política, scheduler, single-flight, carreras y stale-on-error.
4. **Fase 7 en paralelo:** tray.
5. **Fase 4:** streams en memoria.
6. **Fase 3:** diff sólo si hay evidencia de coste/parpadeo del modelo.
7. **Fase 5:** optimización de imágenes guiada por métricas.
8. **Fase 6:** persistencia de vista sólo si aporta valor demostrable.

Cada fase debe poder validarse y descartarse por separado. No actualizar dependencias grandes ni reorganizar módulos mientras se mide una de estas variables.

---

## 8. Métricas de aceptación

Los umbrales temporales se aplican a una máquina y perfil de referencia, no como garantía universal.

- **Proceso frío, cachés calientes:** primera fila con contenido < 2 s y filas precargadas estables < 5 s en el entorno de referencia; 30/30 ejecuciones sin skeleton permanente.
- **SQLite:** cero antiguos timeouts que fuercen red; lecturas >500 ms investigadas y sin bloqueos repetidos en el arranque.
- **Caché:** ≥90 % de hits para recursos efectivamente ya vistos y aún reutilizables.
- **Revalidación:** fresh hit sin red; en addons que ofrecen validadores, medir 304 por addon sin imponer un 80 % global.
- **Consistencia:** una respuesta antigua nunca sobrescribe una nueva; respuesta vacía válida y errores se comportan según la matriz.
- **Detalles:** meta cacheada visible <300 ms; streams cacheados en sesión <1 s en el entorno de referencia.
- **Imágenes:** sin descargas repetidas en segundo arranque; decodificación limitada a lo visible más margen; mejora medida antes/después.
- **Tray:** mismo tamaño/maximizado al volver; minimizada restaurada; una transición por clic; sin remapeos repetidos ni regresión PiP.

Además de medias, guardar p50/p95 y el peor caso. Comparar bytes de red, número de requests, escrituras SQLite y decodificaciones por arranque.

---

## 9. Estado de implementación (2026-11-04)

### Implementado

- **Tray:** estado de restauración capturado antes de ocultar, generación para cancelar retries antiguos, reaplicación de tamaño/maximizado/fullscreen tras `show()`, remapeo de superficie en Wayland cuando la ventana está visible pero no enfocada, y foco reintentado sin sleeps bloqueantes.
- **Caché HTTP:** política por recurso y presencia de validador, scheduler background con concurrencia máxima de 2 y jitter, single-flight para misses, protección por generación contra respuestas antiguas, stale-on-error, backoff exponencial por host y métricas agregadas.
- **Streams:** caché exclusivamente en memoria, TTL 2 minutos, máximo 64 entradas/8 MB, clave aislada por perfil, single-flight y sin persistencia de URLs firmadas.
- **Imágenes:** métricas de hits, descargas, fallos, decodificaciones y tiempo de disco/decodificación; resumen a los 15 s y al cierre.
- **Catálogos:** `models/board::sync` ya actualiza filas in-place y omite secciones cuyo fingerprint no cambió. No se añadió un segundo sistema de diff; la Fase 3 queda como optimización condicionada a que las métricas demuestren que la proyección existente sigue siendo costosa.
- **Benchmark de pool:** `STREMIO_DB_POOL_CONNECTIONS` permite comparar 2/4/8 conexiones (por defecto 8, rango 1–32) sin recompilar.
- **WIP:** build release completado; se eliminaron los dos warnings de `core-env` y se mantiene la compilación de `stremio-native`.

### Pendiente de validación

- Ejecutar la matriz de tray en Wayland/X11 y la prueba manual de Windows.
- Benchmark de 20–30 arranques para confirmar la reducción de lecturas lentas.
- Comparar pools de 2/4/8 conexiones si el benchmark muestra contención.
- Fases 3, 5 y 6 permanecen condicionadas a evidencia de parpadeo, coste de decodificación o necesidad real de restaurar la vista. No se implementaron porque el roadmap exige medirlas primero.

### Validación inicial en debug

Dos procesos nuevos consecutivos con el mismo perfil y caché caliente:

- primer arranque: `hit_fresh=9`, `miss=0`, `revalidate_scheduled=5`, `revalidate_304=2`, `slow_reads=0`;
- segundo arranque: `hit_fresh=17`, `miss=3`, `revalidate_scheduled=2`, `revalidate_304=1`, `revalidate_200=1`, `response_bytes=155801`, `slow_reads=0`;
- imágenes en ambos: 21 lecturas de disco/decodificaciones, sin descargas; `decode_ms≈1.4 s` acumulados.

Esto confirma que ya no aparecen los antiguos timeouts, que la mayoría de lecturas son hits y que la revalidación dejó de ser masiva. También deja una señal para la Fase 5: se siguen decodificando los mismos pósters en cada proceso, aunque no se descargan.

Validación sobre el binario **release** (4 procesos consecutivos, mismo perfil):

- `slow_reads=0` y `old_timeouts=0` en las 4 ejecuciones;
- `hit_fresh` entre 9 y 17; `miss` entre 0 y 11 según qué recursos pedía cada sesión;
- `revalidate_304` entre 0 y 2 y `revalidate_200` entre 0 y 7; sin errores de red;
- `stream_hit` ya aparece (2 en una ejecución), confirmando que la caché de sesión de streams se usa;
- imágenes: entre 21 y 182 decodificaciones por arranque, `decode_ms` acumulado de 222 a 2633 ms, con 0–2 descargas. La lectura de disco se mantiene baja; el coste restante es decodificación.

Con `STREMIO_DB_POOL_CONNECTIONS=2` tampoco aparecieron lecturas lentas en la prueba corta, así que el pool de 8 no es imprescindible para este perfil, pero tampoco introduce regresiones. El siguiente cuello de botella medido es la decodificación repetida de imágenes, no SQLite.

### Cómo recoger la evidencia

```bash
# Un resumen HTTP aparece a los 15 s y al cierre:
grep -E "HTTP cache metrics|image cache metrics|slow HTTP cache read" \
  "$XDG_DATA_HOME/stremio-native/logs/stremio.log"

# Comparar presión de SQLite sin recompilar:
STREMIO_DB_POOL_CONNECTIONS=2 ./stremio-native
STREMIO_DB_POOL_CONNECTIONS=8 ./stremio-native

# Telemetría de arranque existente (opcional):
STREMIO_STARTUP_TELEMETRY=1 ./stremio-native
```

Para cada ejecución anotar: `hit_fresh`, `miss`, `revalidate_304`, `revalidate_200`, `network_error`, `stale_served_on_error`, bytes, lecturas lentas y métricas de imagen. Comparar p50/p95, no sólo el promedio.

## 10. Estado del working tree

Cambios existentes sin commitear de la iteración anterior:

- pool de conexiones Turso en `core-env/src/lib.rs`;
- eliminación del timeout de 2 s y log de lectura lenta en `core-env/src/http_cache.rs`;
- liberación de la conexión antes del llavero;
- etiqueta de decodificador por plataforma;
- toggle de tray con debounce en Windows y retry de foco.

El build release **ya terminó correctamente** y el binario contiene el log `slow HTTP cache read`. Queda pendiente la validación runtime/estadística. `core-env` emitió dos warnings, incluido un `mut` innecesario, que se revisarán al ejecutar Fase 0.

Este roadmap no autoriza borrar cachés o perfiles reales, persistir streams, hacer prefetch de streams al hover ni introducir los cambios invasivos de Fases 3/5/6 sin evidencia previa.
