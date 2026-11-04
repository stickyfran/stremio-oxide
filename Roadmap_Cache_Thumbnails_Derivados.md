# Roadmap: caché persistente de thumbnails derivados

**Fecha:** 2026-11-04  
**Estado:** implementado, compilado y con tests unitarios aprobados; pendiente benchmark en ejecución.  
**Alcance:** Fase 5 del roadmap de caché inteligente, limitada a imágenes.  
**Documento relacionado:** `Roadmap_Cache_Inteligente_y_Tray.md`.

> Este roadmap desarrolla una idea conceptual. Las resoluciones, tamaños de archivo y tiempos de decodificación usados como ejemplos en la inspiración original **no son requisitos ni resultados garantizados**. Los valores definitivos saldrán de la UI, el DPI y las mediciones del equipo de referencia.

---

## 1. Objetivo

Evitar que cada proceso vuelva a decodificar y redimensionar la imagen fuente cuando la UI necesita repetidamente una misma representación pequeña.

Se añadirá una **caché de derivados comprimidos**, separada de la caché de fuentes y de la caché RGBA en memoria:

1. obtener la fuente desde red o desde su caché actual;
2. decodificarla y ajustarla a la variante que necesita la UI;
3. publicar inmediatamente el bitmap resultante en la caché RAM existente;
4. después, fuera del camino crítico, comprimir ese mismo resultado y guardarlo como derivado;
5. en una petición posterior —incluido otro arranque— leer el derivado, decodificarlo directamente y ponerlo en RAM, sin decodificar la fuente grande ni repetir el downscale.

El objetivo no es eliminar toda decodificación: un JPEG, PNG o WebP derivado también debe decodificarse. Se busca que ese trabajo sea sobre pocos píxeles y sin un redimensionado posterior.

### Resultado que se quiere observar

- Menos decodificaciones de fuentes en arranques con caché caliente.
- Menor tiempo acumulado de decodificación y menor tiempo hasta contenido estable.
- Más imágenes útiles dentro del presupuesto RAM ya existente.
- Menos bytes leídos respecto de persistir RGBA crudo.
- Sin aumentar el límite de RAM ni degradar visiblemente la calidad.
- Sin penalizar de forma apreciable la primera aparición de una imagen por esperar al encoder.

---

## 2. Situación actual y brecha

### Ya existe en `app/src/image_cache.rs`

- Caché RAM de bitmaps RGBA con límite de **48 MB**, TTL de inactividad y evicción.
- Caché de fuentes en disco con límite total actual de **384 MB**, retención, sharding y escritura atómica.
- Optimización de algunas URLs para solicitar recursos remotos más pequeños.
- Downscale después de decodificar cuando la imagen supera los límites actuales.
- Workers y semáforos acotados para red, disco y decodificación.
- Cola LIFO para priorizar peticiones recientes/visibles.
- Métricas agregadas de hits, descargas, fallos, decodificaciones, lectura y decodificación.

### Lo que falta

- Persistir el resultado redimensionado como un archivo comprimido independiente.
- Consultar ese derivado antes que la fuente en arranques siguientes.
- Identificar RAM y disco por **fuente + variante**, no sólo por URL.
- Pasar a la caché el rol y tamaño físico realmente requeridos por la UI.
- Separar métricas de fuente y derivado para poder demostrar la mejora.

### Restricciones importantes del código actual

1. La caché RAM está indexada sólo por URL optimizada. Introducir variantes sin cambiar la clave mezclaría imágenes incompatibles.
2. El rol se infiere actualmente de la orientación de la fuente. Una imagen apaisada puede terminar tratada como backdrop aunque se solicite como logo; el rol de uso debe venir del llamador.
3. Las APIs actuales reciben principalmente URL, no tamaño objetivo. Antes de activar derivados hay que definir cómo llega ese dato sin bloquear el modelo ni acoplar el worker a Slint.
4. La extensión histórica de la fuente no garantiza su formato real. La detección por contenido debe conservarse.
5. Las notificaciones de refresco también están asociadas a URL; deberán actualizar la variante correcta o invalidar coherentemente todas sus variantes.

---

## 3. Invariantes de diseño

Estas reglas no se negocian durante la implementación:

- **No persistir RGBA crudo.** En disco sólo se guardan fuentes y derivados comprimidos.
- **No reemplazar la fuente por el thumbnail.** La fuente permite generar otras variantes y recuperarse de cambios de UI, DPI, formato o calidad.
- **No aumentar el límite RAM actual como parte de esta mejora.** Los bitmaps derivados comparten el mismo presupuesto de 48 MB.
- **No codificar en el camino crítico.** Primero se entrega el bitmap a RAM/UI; el encoding se programa después.
- **No usar tamaños universales fijados por este documento.** El objetivo se calcula a partir de la superficie real y del factor de escala físico.
- **No mezclar roles, tamaños o políticas de resize.** Cada variante tiene identidad propia.
- **Un derivado inválido nunca provoca una descarga si la fuente local es válida.** Se descarta y se regenera desde la fuente.
- **Un fallo de escritura del derivado no invalida una imagen ya decodificada.** La UI conserva el resultado y puede reintentarlo en otra ocasión.
- **No prometer latencias, ratios de compresión ni ahorros fijos.** Se aceptan o rechazan con benchmarks.
- **No registrar URLs completas, tokens ni datos de perfil en las nuevas métricas.**

---

## 4. Modelo de datos y claves

### 4.1 Identidad de la fuente

Conservar la clave y los archivos existentes basados en la URL optimizada. Esto permite reutilizar la caché actual y evita una migración destructiva.

Conceptualmente:

```text
SourceKey = hash(canonical_optimized_url)
```

La fuente sigue siendo una única entrada por representación remota, independientemente de cuántas variantes se deriven de ella.

### 4.2 Identidad del derivado

```text
VariantKey = hash(
    SourceKey,
    role,
    target_width_px,
    target_height_px,
    resize_or_fit_policy,
    derived_format_version
)
```

La versión de formato debe cambiar cuando cambie cualquier decisión que altere los bytes o la interpretación del derivado: algoritmo, redondeo, codec, calidad o tratamiento de alpha.

- Los nombres de archivo contienen hashes y metadatos no sensibles, nunca la URL completa.
- La serialización de la clave debe ser canónica y sin ambigüedad de separadores.
- La caché RAM usa la misma identidad de variante, aunque no necesariamente la misma representación de clave que el nombre de archivo.
- La clave de red/estado de descarga puede seguir siendo por fuente: una sola descarga puede alimentar varias variantes.
- Debe existir single-flight por variante para no decodificar/comprimir el mismo derivado varias veces.

### 4.3 Tamaño objetivo

```text
target_px = tamaño_del_área_de_imagen_en_UI × escala_física_actual
```

Se aplican las reglas de aspect ratio ya elegidas por la UI y límites defensivos de recursos. No se agrandan fuentes pequeñas salvo una decisión explícita de render.

En la primera versión no se redondean arbitrariamente tamaños ni se agrupan variantes “parecidas”: eso podría causar blur o multiplicar entradas. Cualquier cuantización posterior requiere medición visual y de hit rate.

Al cambiar DPI, monitor o tamaño de ventana:

- se calcula la nueva variante;
- se reutiliza si ya existe;
- si no, se genera desde la fuente;
- no hace falta vaciar toda la caché; la capacidad y el mantenimiento retiran variantes antiguas.

### 4.4 Organización en disco

Separar físicamente las familias, por ejemplo:

```text
image_cache/
  sources/<shard>/<source_hash>...
  derived/<shard>/<variant_hash>.<ext>
```

La distribución exacta se decide al implementar, pero fuentes y derivados deben poder mantenerse y presupuestarse por separado. Los derivados pueden sobrevivir temporalmente a una fuente evictada y seguir siendo utilizables; si falta el derivado y también la fuente, se usa el flujo normal de red.

---

## 5. Pipeline objetivo

### 5.1 Petición con derivado caliente

```text
UI(role + tamaño físico)
  → VariantKey
  → RAM hit → dibujar
  → RAM miss → derivado en disco
  → validar variante, dimensiones y bytes
  → decodificar archivo pequeño directamente a RGBA
  → insertar en RAM
  → dibujar
```

No se ejecuta downscale si el archivo coincide con la variante solicitada. Si la validación falla, se elimina el derivado corrupto/obsoleto y se pasa a la fuente.

### 5.2 Petición sin derivado, con fuente local

```text
UI → RAM miss → derivado miss
  → leer fuente local
  → decodificar fuente + ajustar a la variante
  → insertar resultado en RAM y notificar a UI
  → encolar encoding/escritura del derivado en background
```

No hay red si la fuente sigue siendo válida.

### 5.3 Petición fría

```text
UI → RAM miss → derivado miss → fuente miss
  → descargar una vez
  → persistir fuente
  → decodificar + ajustar a la variante
  → insertar en RAM y notificar a UI
  → encolar encoding/escritura del derivado en background
```

La primera vista no espera al encoder del derivado. Persistir la fuente y generar el derivado no deben duplicar la descarga.

### 5.4 Variantes concurrentes de una misma fuente

Se deduplica la descarga por `SourceKey`, pero cada variante conserva su propio trabajo de decodificación/encoding y su entrada RAM. No se debe mantener una descarga “en progreso” que impida servir otra variante ya disponible en disco.

---

## 6. Formato y calidad

La elección depende del contenido real, no de la extensión de la URL:

- **Imágenes opacas:** evaluar JPEG de alta calidad como primera opción medible.
- **Imágenes con alpha significativo:** usar un formato que conserve transparencia; PNG o WebP lossless son candidatos con las dependencias actuales.
- El encoder WebP disponible mediante `image 0.25` es lossless; no asumir que producirá archivos pequeños equivalentes a un WebP lossy.
- No eliminar alpha por haber elegido un rol normalmente opaco: comprobar los píxeles o conservar una señal fiable de decodificación.
- No introducir otra biblioteca de codecs en el primer incremento salvo que los resultados justifiquen el cambio de dependencias.

La calidad se valida con pósters, backdrops, texto, logos finos, bordes y transparencias. Un archivo más pequeño no es una mejora si introduce artefactos visibles.

---

## 7. Roadmap por entregas

### Etapa 0 — Baseline y preparación de medición

**Objetivo:** poder comparar exactamente el mismo recorrido antes y después, sin tocar todavía el formato de caché.

1. Fijar una copia de perfil de referencia y un recorrido reproducible de Board, detalles, scroll y logos.
2. Registrar cuatro escenarios separados:
   - cachés completamente vacíos;
   - fuentes calientes, sin derivados —equivalente al comportamiento actual/migración—;
   - derivados calientes;
   - red desconectada con caché local.
3. Guardar por ejecución: imágenes solicitadas, hits RAM, hits de fuente, descargas, bytes de disco, decodificaciones, tiempo de decodificación y tiempo hasta contenido estable.
4. Añadir p50/p95 y peor caso por imagen cuando sea posible, además de los agregados del proceso.
5. Separar `source_decode` y `derived_decode` desde la instrumentación; ambos son decodificaciones, no ocultar el segundo dentro de un “hit” genérico.
6. Usar como referencia histórica los 21–182 decodes y aproximadamente 222–2633 ms acumulados ya observados en release, sin convertirlos en SLO universal.

**Done:** baseline repetible, sin mezclar primer arranque con arranque caliente ni red con CPU.

### Etapa 1 — Contrato de variantes, sin cambiar todavía la persistencia

**Objetivo:** resolver identidad, rol, tamaño y refresco antes de escribir derivados.

1. Auditar todos los llamadores de `get_poster_image`, `get_cached_image`, `request_image`, `cover_art_file` y usos equivalentes.
2. Introducir un contrato pequeño de solicitud con fuente, rol, tamaño físico y política de ajuste.
3. Propagar el rol real desde la UI; dejar de depender de la orientación como sustituto del uso.
4. Calcular el target en la frontera UI/adaptador, no consultando ventanas desde workers.
5. Cambiar la clave RAM y el refresco para distinguir variantes, conservando un fallback compatible durante la migración.
6. Mantener cover art/SMTC como consumidor separado: puede reutilizar fuentes, pero no debe recibir por accidente un thumbnail de tarjeta.
7. Añadir pruebas de claves y de aislamiento antes de activar derivados.

**Done:** la misma URL solicitada como dos roles/tamaños produce dos entradas correctas, sin mezclarse, y el comportamiento de disco sigue siendo el actual.

### Etapa 2 — Vertical slice: escritura diferida para un rol

**Objetivo:** validar el mecanismo completo en un alcance pequeño antes de generalizarlo.

1. Elegir póster como primer rol si el recorrido de benchmark lo representa adecuadamente.
2. Después de insertar el bitmap en RAM y solicitar el refresco de UI, encolar su encoding.
3. Usar una cola acotada y pocos workers; no competir con decodificaciones foreground ni saturar todos los núcleos.
4. Deduplicar trabajos por `VariantKey` y descartar trabajos obsoletos antes de comenzar cuando sea posible.
5. Escribir con archivo temporal + rename atómico, igual que la política actual, y limpiar temporales en error.
6. Clonar de forma barata el buffer compartido para el worker; no copiar RGBA adicional innecesariamente ni retener buffers durante toda la sesión.
7. No bloquear cierre de la aplicación esperando una cola grande de derivados.

**Done:** una imagen se muestra sin esperar al encoder y aparece un derivado válido y completo; un error de encoding no afecta a la imagen mostrada.

### Etapa 3 — Lectura derivado-primero y fallback seguro

**Objetivo:** obtener el beneficio en el siguiente proceso.

1. Consultar RAM y después el derivado exacto.
2. Validar tamaño esperado, formato, límites defensivos y versión antes de aceptarlo.
3. Decodificar directamente al bitmap final, sin pasar por el resize de fuentes.
4. Ante archivo ausente, corrupto o incompatible: fuente local → red, en ese orden.
5. No registrar un derivado corrupto como fallo de red ni repetir infinitamente su regeneración en una misma sesión.
6. Integrar backdrops y logos sólo después de validar el vertical slice de pósters.

**Done:** al reiniciar con derivados calientes, las métricas muestran `derived_hit` y `derived_decode`, sin `source_decode` ni downscale para esas imágenes; la calidad visual es equivalente.

### Etapa 4 — Presupuestos, mantenimiento y ciclo de vida

**Objetivo:** impedir que la reutilización se convierta en crecimiento ilimitado de disco o colas.

1. Mantener intactos los 48 MB de RAM.
2. Definir un presupuesto de derivados separado del presupuesto de fuentes. El valor inicial se elegirá tras medir una sesión representativa; no se adopta automáticamente una cifra de ejemplo.
3. Registrar bytes y entradas por familia, evicciones, expiraciones y huérfanos.
4. Aplicar retención y evicción por utilidad/recencia aproximada; no escanear todo el árbol en cada petición.
5. Programar mantenimiento fuera del camino UI y evitar que borre un archivo que está siendo escrito.
6. Al superar presupuesto, eliminar primero derivados regenerables y antiguos antes que fuentes útiles.
7. Limpiar derivados expirados aunque la fuente siga viva y viceversa, sin asumir que ambos archivos existen siempre.
8. Invalidar mediante versión los derivados afectados por cambios de algoritmo/formato, sin borrar la caché del usuario durante una actualización normal.

**Done:** ambos presupuestos permanecen acotados tras navegación repetida, cambios de tamaño/DPI y reinicios; no hay crecimiento sostenido de temporales o colas.

### Etapa 5 — Benchmark comparativo, calidad y cierre

**Objetivo:** aceptar la mejora con datos o revertir el mecanismo si no compensa.

1. Repetir exactamente la Etapa 0 con la implementación nueva.
2. Comparar en el mismo equipo y perfil:
   - tiempo hasta primera imagen y contenido estable;
   - `source_decode` frente a `derived_decode`;
   - tiempo de decode p50/p95 y acumulado;
   - tiempo y bytes de lectura;
   - latencia de cola, tiempo de encoding y bytes escritos;
   - descargas, fallos/fallbacks y entradas RAM efectivas;
   - disco total por fuentes y derivados.
3. Revisar el coste del primer uso: encoding diferido, CPU total y posible impacto en scroll.
4. Hacer inspección visual a escalas y DPI representativos, incluyendo alpha y cambios de tamaño en caliente.
5. Probar suspensión/cierre durante encoding y archivos incompletos.
6. Documentar resultados y elegir calidad/presupuesto definitivos.

**Done:** mejora clara en arranque con derivados calientes, sin regresión significativa en primer uso, calidad, RAM ni estabilidad.

---

## 8. Métricas nuevas

Todas agregadas; sin URL ni información sensible:

| Métrica | Qué demuestra |
|---|---|
| `derived_hit` / `derived_miss` | Reutilización real del nuevo formato |
| `source_hit_after_derived_miss` | Regeneración sin red |
| `source_decode` / `derived_decode` | Trabajo grande evitado frente a decode pequeño restante |
| `source_downscale` | Downscales efectivamente evitados en hits derivados |
| `derived_decode_micros` | Coste de leer el nuevo formato |
| `derived_encode_micros` | Coste diferido de crearlo |
| `derived_queue_micros` | Retraso/background pressure del encoder |
| `derived_read_bytes` / `derived_write_bytes` | I/O adicional y ahorro relativo |
| `derived_corrupt` / `derived_fallback` | Seguridad del fallback |
| `derived_evictions` / `derived_disk_bytes` | Cumplimiento del presupuesto |
| `memory_entries` / `memory_bytes_estimate` | Mejor aprovechamiento del mismo límite RAM |

El resumen debe permitir comparar tiempos de arranque sin confundir tiempo de CPU acumulado con latencia de pared.

---

## 9. Pruebas obligatorias

### Unitarias

- Clave estable y distinta por URL, rol, tamaño, fit y versión.
- Cálculo de píxeles físicos con distintos DPI y límites.
- Selección de formato opaco/alpha y conservación de transparencia.
- Aspect ratio y no upscaling involuntario.
- Validación de dimensiones/formato de un derivado.
- Presupuesto y orden de evicción con archivos sintéticos.

### Integración

- Fuente local + derivado ausente → decodifica fuente, muestra y después escribe derivado.
- Derivado caliente → no abre la fuente, no ejecuta downscale y sí llena RAM.
- Derivado corrupto/truncado → lo elimina y usa fuente, sin red.
- Fuente y derivado ausentes → una descarga y una notificación por variante.
- Dos peticiones idénticas concurrentes → un trabajo de fondo, no duplicados.
- Dos variantes de una URL → no se pisan en RAM ni en disco.
- Cambio de DPI/tamaño → variante nueva; la antigua no se muestra como si fuera válida.
- Encoder fallido o disco lleno → imagen visible y fuente intacta.
- Cierre durante escritura → ningún archivo final parcialmente válido.
- Cache antigua anterior a esta función → migración transparente sin borrar fuentes.
- Cover art/SMTC → conserva sus requisitos y no consume una variante incorrecta.

### Exploratorias/visuales

- Pósters con texto pequeño, backdrops detallados, logos finos y alpha.
- Scroll rápido mientras el encoder trabaja.
- Cambio de monitor/DPI y resize continuo.
- Red offline con derivado, sólo fuente y sin ninguna caché.
- Varios reinicios para comprobar que no se regeneran derivados en cada proceso.

Las pruebas usarán imágenes sintéticas y perfiles copiados; nunca se borrará la caché real del usuario.

---

## 10. Criterios de aceptación y rechazo

### Aceptar si

- En arranques calientes disminuyen de forma consistente las decodificaciones de fuente y el downscale.
- Mejora el tiempo de decodificación y/o el tiempo hasta contenido estable en el benchmark acordado.
- No se incrementa el presupuesto RAM y el disco derivado permanece bajo su propio límite.
- El primer uso no queda bloqueado por encoding y no hay regresión perceptible de scroll/UI.
- La calidad se mantiene en los casos visuales acordados.
- Los fallos de derivado se recuperan localmente y de forma segura.

### Ajustar o revertir si

- El coste de encoding supera el ahorro durante el uso real.
- La multiplicación de variantes hace que el hit rate sea demasiado bajo.
- Los archivos derivados no reducen suficientemente I/O o disparan mantenimiento.
- La compresión introduce artefactos visibles.
- La propagación de tamaños genera churn continuo durante resize/scroll.

El feature debe poder desactivarse para volver al camino fuente → decode → RAM sin migrar ni borrar datos.

---

## 11. Orden ejecutivo resumido

```text
Baseline
  → contrato de variantes y claves RAM
  → escritura diferida para pósters
  → lectura derivado-primero + fallbacks
  → backdrops/logos
  → mantenimiento y presupuestos
  → benchmark/calidad
  → adopción, ajuste o rollback
```

No empezar por elegir una resolución, un codec ni un tamaño de caché. El primer entregable técnico es la **identidad correcta de la variante**; el primer beneficio medible aparece al completar la lectura derivado-primero.

---

## 12. Fuera de alcance

- Persistir RGBA crudo.
- Eliminar la caché de fuentes.
- Aumentar la caché RAM para simular una mejora.
- Cambiar HTTP, catálogos, streams o tray como parte de este trabajo.
- Añadir prefetch amplio de imágenes fuera del viewport.
- Introducir una base de datos nueva si los archivos + métricas existentes son suficientes.
- Garantizar tiempos sub-milisegundo o tamaños concretos de thumbnail.
- Reescribir el pipeline completo de imágenes antes de validar un vertical slice.

---

## 13. Estado

**Aprobado conceptualmente:** conservar fuentes, derivar al tamaño real de UI/DPI, almacenar el derivado comprimido, codificar después de publicar la imagen y reutilizarlo en el siguiente arranque.

**Implementado en el working tree (compilado; 13/13 tests de `image_cache`):**

- Roles explícitos `Poster`, `Logo`, `Backdrop`, `Thumbnail` y `Avatar`.
- Variante por URL optimizada + rol + tamaño físico + versión de formato.
- Tamaño calculado con la unidad del tema y el `scale_factor` de la ventana.
- Clave de RAM independiente por variante y un índice fuente/rol para el refresco de la UI.
- Caché de derivados separada de las fuentes, bajo `image_cache/derived/`.
- Lectura derivado-primero con validación de dimensiones y fallback a fuente local y después red.
- Descarga de fuente single-flight entre variantes.
- Encoding diferido, cola acotada y deduplicada por variante.
- JPEG calidad 88 para imágenes opacas; PNG cuando hay alpha real.
- Presupuesto de derivados de 64 MB, retención y mantenimiento separados.
- Métricas de hits/misses de derivado, decodes separados, encoding, bytes, fallos y evicciones.
- Callers principales actualizados para logos, backdrops, miniaturas, avatares y pósters.
- Tests unitarios agregados para claves, rutas, tamaños, formatos y rechazo de derivados inválidos.

**Validación ya realizada:**

- `cargo check -p stremio-native --bin stremio-native --offline`: correcto.
- `cargo test -p stremio-native --bin stremio-native --offline`: **171/171**.
- `cargo test -p core-env --offline`: **18/18**.
- Los 13 tests de `image_cache` incluyen claves por variante, rutas versionadas, límites de tamaño, no-upscaling, JPEG opaco, PNG con alpha y rechazo de derivados inválidos.

**Pendiente antes de dar la fase por cerrada:**

1. Medir dos arranques consecutivos y comparar `source_decode`, `derived_decode`, `decode_ms`, descargas, bytes de disco y tiempo a contenido estable.
2. Revisar visualmente pósters, backdrops, logos con alpha y cambios de DPI.
3. Ajustar presupuesto/calidad solo con datos del benchmark.
