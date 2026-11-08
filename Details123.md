# Roadmap de Rediseño de UI & UX: Alineación de Vistas, Panel de Preview, Transiciones Cinematográficas e Icono de Tray Blanco (Details123.md)

> [!NOTE]
> Este documento técnico y de diseño audita las capturas de pantalla de **Biblioteca** y **Descubrir**, incorporando los resultados de la auditoría técnica en Slint: prevención de *reflow stutter* por re-layout dinámico del `ListView`, optimización del reciclaje de nodos en listas virtualizadas, eliminación de repintados infinitos a 60/120 FPS (*zero unnecessary GPU/CPU cycles*), y compatibilidad multiplataforma del icono del tray en temas oscuros y claros.

---

## 1. Auditoría Visual Forense (Análisis de Capturas)

A continuación se presentan las dos capturas de pantalla de referencia aportadas para la auditoría:

````carousel
![Biblioteca - Estado Actual](/home/kes/.gemini/antigravity/brain/c7ec3a5e-9cef-406a-92f9-ce8405ae1513/screenshot_library.png)
<!-- slide -->
![Descubrir - Estado Actual](/home/kes/.gemini/antigravity/brain/c7ec3a5e-9cef-406a-92f9-ce8405ae1513/screenshot_discover.png)
````

### 1.1. Discrepancias Geométricas y de Alineación Identificadas

1. **Desconexión Vertical y Horizontal de la Barra de Búsqueda:**
   - La barra de búsqueda superior (`SearchBox` en `app/ui/app.slint:1376`) está centrada globalmente en el top navbar mediante `alignment: center;` con un ancho fijo de `28 * root.ui-unit` (aprox. 448px).
   - En la página de **Biblioteca**, la fila de filtros comienza con un padding de `1.5 * root.unit` a la izquierda del contenido (`x = 0` relativo a la página).
   - En la página de **Descubrir**, los desplegables (*Película*, *Populares*, *Género*) inician con `padding: 1.5 * root.unit;` dentro del catálogo izquierdo.
   - **Problema:** La barra de búsqueda "flota" en un eje arbitrario en la cabecera que no guarda relación con la primera columna de tarjetas ni con los filtros inferiores. Cuando el panel lateral de detalles (*Preview Drawer*) se despliega en **Descubrir**, la cuadrícula de tarjetas se comprime hacia la izquierda, pero la barra de búsqueda sigue estática en el medio de toda la pantalla, desbalanceando completamente la composición visual.
   - **Objetivo Requerido:** La barra de búsqueda, los filtros de categoría y el inicio de la cuadrícula de tarjetas deben compartir el mismo eje vertical izquierdo y estar armónicamente integrados.

2. **Inconsistencias en el Botón "Show Preview" en Biblioteca:**
   - En la captura de Biblioteca, el botón `Show Preview` se encuentra en la misma fila de filtros pero separado por un `spacer` hacia la derecha.
   - **Desalineación vertical y estilo:** La altura y el baseline del texto dentro del botón no se alinean exactamente con los botones de ordenamiento adyacentes (`Last Watched`, `A–Z`, etc.).
   - **Localización ausente:** El texto está fijado en inglés `"Show Preview"` / `"Hide Preview"` en `app/ui/pages/library.slint:257` sin traducción al español (*"Mostrar vista previa"* / *"Ocultar vista previa"*).
   - **Ubicación errónea de control:** El botón para alternar o invocar la preview se encuentra en la cabecera izquierda/central de filtros en vez de estar posicionado en el extremo derecho (donde el drawer de preview efectivamente aparece o se expande).
   - **Conflicto del botón de cierre (`x`):** En el componente `MediaPreviewDrawer` (`app/ui/components/preview_drawer.slint:240`), el botón de la cruz (`x`) actualmente sólo cierra el drawer pero deja al usuario desorientado sobre cómo volver a abrirlo, mientras que en Biblioteca existe un botón redundante en la barra de filtros. La acción de cerrar (`x`) debe integrarse formalmente con el estado de "Desactivar/Ocultar preview", y el disparador de apertura debe vivir en el extremo derecho como un botón de anclaje contextual.

3. **Carencia de Transición al Scrollear y Cargar Nuevas Tandas:**
   - Al llegar al final de la cuadrícula en **Biblioteca** o **Descubrir** (`ListView` con `MediaCardGridRow`), las nuevas filas cargadas aparecen de golpe ("pop-in" instantáneo sin interpolación).
   - No hay tarjetas esqueleto animadas (*skeleton loading cards*) en la siguiente tanda que hagan un fade-in suave mientras se reciben los metadatos o las imágenes.
   - En el **Inicio (Board)**, cuando los catálogos (*Popular Movies*, *Popular Series*, o plugins externos) cargan o se actualizan, las tarjetas reemplazan su estado o descargan contenido sin un fundido suave (*fade-in / fade-out*), lo que genera saltos bruscos en lugar de una experiencia fluida y cinematográfica.

4. **Icono de la Bandeja del Sistema (Tray) Desactualizado / Color Incorrecto:**
   - Actualmente, `AppTray` en `app/ui/app.slint:102` y Windows tray cargan `app_icon_window.png` (el logo estándar con degradado azul/morado).
   - En barras de tareas oscuras modernas (GNOME, KDE Plasma, Windows 11 taskbar), los iconos de estado deben ser **monocromáticos en color blanco puro (`#FFFFFF`)** para mantener el estándar visual del sistema operativo y no desentonar con el resto de indicadores.
   - Ya existe un vector base listo `app/ui/assets/images/tray-icon.svg` con la silueta exacta en blanco (`fill="#ffffff"`), pero el componente de tray en Slint (`AppTray`) sigue apuntando a `app_icon_window.png`.

---

## 2. Arquitectura de la Solución Propuesta

```mermaid
flowchart TD
    subgraph Layout["Estructura de Layout & Alineación"]
        TopNav["Top Navigation Bar (app.slint)"]
        SearchBar["SearchBox Integrado"]
        FiltersRow["Barra de Filtros Unificada (Library / Discover)"]
        GridArea["Cuadrícula de Tarjetas (ListView Virtualizado)"]
        PreviewArea["MediaPreviewDrawer (Lado Derecho)"]
        
        TopNav --> SearchBar
        SearchBar -.->|Alineación X compartida (1.5 * ui-unit)| FiltersRow
        FiltersRow -.->|Alineación X compartida| GridArea
        PreviewArea -.->|Offset estático por preview-enabled (sin reflow stutter)| GridArea
    end

    subgraph PreviewUX["Ciclo de Vida de Preview Drawer"]
        ToggleBtn["Botón 'Mostrar Vista Previa' (Extremo Derecho)"]
        DrawerSlide["Drawer con transición X/Opacidad (200ms)"]
        CloseBtn["Botón Cruz 'X' (Header Drawer)"]
        
        ToggleBtn -->|Click (preview-enabled = true)| DrawerSlide
        CloseBtn -->|Click (preview-enabled = false)| ToggleBtn
    end

    subgraph Motion["Sistema de Animaciones de Alto Rendimiento (Zero Loop Overhead)"]
        ScrollTrigger["Trigger de Paginación (25 unit viewport margin)"]
        SkelStaticFade["Tanda Esqueleto: Fade-in estático a color neutro (180ms - sin bucle continuo)"]
        ImageDecoder["Carga / Decodificación de Póster"]
        PosterTextureCrossFade["Cross-fade de Textura de Póster (image-loaded ? 1.0 : 0.0 en 200ms)"]
        BoardUpdate["Actualización / Recarga en Board"]
        BoardFadeOut["Fade-out breve de ítems salientes (120ms)"]
        BoardFadeIn["Fade-in escalonado de nuevos ítems (200ms)"]

        ScrollTrigger --> SkelStaticFade
        SkelStaticFade --> ImageDecoder
        ImageDecoder --> PosterTextureCrossFade
        BoardUpdate --> BoardFadeOut --> BoardFadeIn
    end

    subgraph TraySystem["Bandeja del Sistema Adaptativa (Dark/Light Safe)"]
        TrayComponent["AppTray (Slint / KSNI / Win32)"]
        AdaptiveIcon["tray-icon.svg (#FFFFFF con sutil borde de contraste / color-scheme)"]
        TrayComponent --> AdaptiveIcon
    end
```

---

## 3. Fases del Roadmap y Correcciones de Auditoría Técnica

### Fase 1: Alineación Geométrica del Buscador, Filtros y Cuadrícula
**Archivos involucrados:**
- [app/ui/app.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint)
- [app/ui/pages/library.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/library.slint)
- [app/ui/pages/discover.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/discover.slint)

1. **Coordinación del Eje X de la Barra de Búsqueda:**
   - Retirar el `alignment: center` estático de la barra de navegación superior.
   - Implementar un cálculo reactivo de márgenes laterales: el contenedor del buscador debe iniciar exactamente en el mismo `x` relativo donde comienzan los `SelectField` de Biblioteca y Descubrir (`padding-left: 1.5 * root.ui-unit` respecto al panel de contenido).
   - En pantallas anchas o cuando el panel de preview no esté abierto, mantener una anchura máxima acotada pero alineada a la izquierda con la cuadrícula de pósters, de modo que la primera tarjeta quede visualmente perpendicular con el inicio del buscador y los filtros.

2. **Alineación con el Panel de Preview:**
   - La métrica de ancho disponible tanto para la cuadrícula como para el delimitador derecho del `SearchBox` se calculará de manera estática y condicional basada estrictamente en el estado booleano objetivo (`preview-enabled ? parent.width - 29 * root.unit : parent.width`).
   - Esto evita que durante la apertura o cierre del panel lateral existan valores intermedios que induzcan recálculos o desfases en la barra de búsqueda y en el cálculo de columnas de la grilla.

---

### Fase 2: Rediseño del Control de Vista Previa (Preview Drawer UX) & Prevención de Reflow Stutter
**Archivos involucrados:**
- [app/ui/pages/library.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/library.slint)
- [app/ui/pages/discover.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/discover.slint)
- [app/ui/components/preview_drawer.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/components/preview_drawer.slint)

1. **Reposicionamiento y Rediseño del Botón de Toggle:**
   - Mover el botón disparador de la preview al **extremo superior derecho** de la pantalla, en la zona natural donde vive el panel lateral de detalles.
   - Cuando el panel está cerrado (`!preview-enabled` o drawer contraído), el botón exhibe el icono de panel lateral con tooltip/etiqueta traducida: `@tr("PREVIEW_SHOW" => "Mostrar vista previa")`.
   - El botón se diseñará con altura estándar (`3.25 * root.unit`), borde redondeado coherente, y contraste alineado con el tema actual.

2. **Unificación de la Cruz (`x`) con el Rol de Desactivar Preview:**
   - El botón `x` situado en la esquina superior derecha del `MediaPreviewDrawer` (`preview_drawer.slint:240`) asumirá formalmente la desactivación de la preview (`preview-enabled = false`).
   - **Corrección de Auditoría (Prevención de Reflow Stutter en ListView):**
     > [!WARNING]
     > Si se interpola continuamente `drawer-width` durante 200ms mientras el ancho de la grilla depende de `parent.width - drawer-width`, el `ListView` recalculará el layout de las filas en cada frame a 60/120 FPS. Si en medio de la animación se cruza el umbral de una columna adicional, se produce un salto brusco (*reflow stutter*).
     - **Solución Técnica:**
       1. Calcular el número de columnas (`column-count`) y la métrica de `grid-width` basándose **estrictamente en el estado booleano objetivo (`preview-enabled`)**, no en el ancho intermedio animado.
       2. Deslizar el `MediaPreviewDrawer` mediante traslación de posición en $X$ (`x: root.preview-enabled ? parent.width - self.width : parent.width`) combinada con un fundido suave de opacidad (200ms).
       3. Aplicar un fade rápido de 150ms al contenedor de la grilla ante el cambio de columnas para una transición fluida y sin costo de layout en cada frame.

3. **Internacionalización Completa:**
   - Reemplazar cadenas hardcodeadas en inglés por llamadas a `@tr()`:
     - `"Show Preview"` $\rightarrow$ `@tr("PREVIEW_SHOW" => "Mostrar vista previa")`
     - `"Hide Preview"` $\rightarrow$ `@tr("PREVIEW_HIDE" => "Ocultar vista previa")`
     - `"Last Watched"` $\rightarrow$ `@tr("SORT_LAST_WATCHED" => "Visto recientemente")`
     - `"A–Z"` / `"Z–A"` / `"Most Watched"` $\rightarrow$ claves traducidas equivalentes.

---

### Fase 3: Sistema de Animación de Carga Escalonada & Cero Ciclos Innecesarios de CPU/GPU
**Archivos involucrados:**
- [app/ui/components/media_card.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/components/media_card.slint)
- [app/ui/components/media_carousel.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/components/media_carousel.slint)
- [app/ui/pages/board.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/board.slint)
- [app/ui/pages/library.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/library.slint)
- [app/ui/pages/discover.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/pages/discover.slint)

1. **Corrección de Auditoría (Reciclaje de Nodos en `ListView` Virtualizado):**
   > [!IMPORTANT]
   > En Slint, un `ListView` virtualizado recicla las instancias de los componentes visuales durante el scroll. Si se anima la propiedad `opacity` general del `MediaCard`, la animación puede dispararse aleatoriamente al reusar tarjetas o no dispararse si el nodo reciclado ya tenía `opacity: 1.0`.
   - **Solución Técnica de Cross-fade:**
     - Separar la opacidad del contenedor estructural de la tarjeta de la opacidad de la **textura del póster**.
     - Vincular la transición de 200ms (`animate opacity { duration: 200ms; easing: cubic-bezier(0.22, 1.0, 0.36, 1.0); }`) exclusivamente a la condición reactiva de textura lista (`poster-ready ? 0.95 : 0.0`).
     - Al reciclarse un nodo, si el nuevo ítem ya tiene póster en caché de memoria, aparece de inmediato sin parpadeos; si el póster está en descarga, muestra el fondo neutro y transiciona suavemente únicamente al llegar los bytes decodificados.

2. **Corrección de Auditoría (Eliminación de Shimmer Infinito en CPU/GPU):**
   > [!CAUTION]
   > Un bucle de animación continuo tipo *shimmer* o pulso sinusoidal infinito en las *Skeleton Cards* fuerza a Slint a repintar continuamente a 60/120 FPS en el hilo de UI/GPU mientras espera la respuesta de red de los addons, violando el mandato de brutal eficiencia en reposo.
   - **Solución Técnica:**
     - Utilizar un **fade-in de entrada estático de 180ms** hacia un fondo sólido neutro (`Theme.card-background` con borde sutil), sin bucles de reloj continuos.
     - Si se incorpora algún efecto pulsante, estará estrictamente acotado:
       - Solo se activa si `visible && loading && !poster-ready`.
       - Se detiene inmediatamente tras un número finito de ciclos (o al resolverse el payload), permitiendo que el motor de Slint duerma completamente cuando no hay interacción del usuario.

3. **Animación en Catálogos de Inicio (Popular, Destacados y Addons):**
   - En `BoardPage`, ante la llegada de nuevos catálogos o reordenamiento:
     - Las tarjetas salientes ejecutan una transición de fade-out breve (`duration: 120ms; opacity: 1 -> 0`).
     - Las tarjetas nuevas entran con fade-in secuencial (`duration: 200ms; opacity: 0 -> 1`).
     - Se mantiene la posición de scroll y la estabilidad geométrica de la fila.

---

### Fase 4: Icono Monocromático de Tray con Soporte para Temas Claros y Oscuros
**Archivos involucrados:**
- [app/ui/app.slint](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint#L101-L105)
- [app/ui/assets/images/tray-icon.svg](file:///home/kes/Documentos/Programando/stremio-native/app/ui/assets/images/tray-icon.svg)
- [app/src/tray/windows.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/tray/windows.rs)
- [app/src/tray/standard.rs](file:///home/kes/Documentos/Programando/stremio-native/app/src/tray/standard.rs)

1. **Corrección de Auditoría (Compatibilidad Multiplataforma y Temas Claros):**
   > [!WARNING]
   > Un icono `#FFFFFF` puro es perfecto en barras de tareas oscuras, pero en Windows 10/11 con barra de tareas clara (modo claro) o en escritorios Linux con paneles claros, un blanco puro se vuelve completamente invisible.
   - **Solución Técnica en el Vector SVG:**
     - Mantener el relleno principal en blanco puro `#FFFFFF`.
     - **Orden de pintado y grosor del Stroke:** En SVG estándar, el stroke se dibuja centrado sobre el borde del path (mitad hacia adentro y mitad hacia afuera), lo que en tamaños pequeños (16x16 o 22x22 px en el tray) puede comerse el grosor de las líneas blancas internas del logo. Para prevenir esto, se especifican los atributos: `paint-order="stroke fill"`, `stroke-linejoin="round"`, `stroke-width="1.5"` y `stroke="rgba(0, 0, 0, 0.45)"`. Así el rasterizador dibuja primero el borde oscuro por detrás y encima el relleno `#FFFFFF` intacto al 100% de su geometría original.
     - Este contorno es invisible sobre fondos oscuros (se funde naturalmente), pero garantiza que en barras de tareas claras (Windows Light Theme, macOS Light Menu Bar, KDE Breeze Light) el imagotipo conserve un contraste y definición impecables.
     - En Linux (vía KSNI / StatusNotifierItem), agregar la clase semántica `class="color-scheme"` para entornos compatibles con iconos simbólicos del sistema.

2. **Integración en `AppTray` y Windows Backend:**
   - En `app/ui/app.slint`:
     ```slint
     export component AppTray inherits SystemTrayIcon {
         icon: @image-url("assets/images/tray-icon.svg");
         title: "Stremio Oxide";
         tooltip: "Stremio Oxide";
         ...
     ```
   - En Windows tray (`app/src/tray/windows.rs`), compilar el icono de notificación con el borde de contraste adaptativo, preservando `app_icon_window.png` a todo color únicamente para la barra de tareas y el icono de la ventana principal.

---

## 4. Matriz de Componentes y Cambios Técnicos

| Componente | Archivo Fuente | Cambio Requerido | Mitigación Técnica / Rendimiento |
| :--- | :--- | :--- | :--- |
| **Top Navigation** | `app/ui/app.slint` | Alinear contenedor de `SearchBox` con el margen izquierdo de las vistas activas (`1.5 * ui-unit`) | Corrige el desfase entre la barra de búsqueda y los filtros |
| **Filtros Biblioteca** | `app/ui/pages/library.slint` | Reubicar botón de preview al extremo derecho, alinear baseline vertical | Elimina desalineación visual en la barra de herramientas |
| **Preview Drawer** | `app/ui/components/preview_drawer.slint` | Enlazar botón `x` al toggle `preview-enabled = false`; animar posición $X$/opacidad | **Previene reflow stutter:** layout no interpola columnas frame a frame |
| **Internacionalización** | `app/ui/pages/library.slint` | Sustituir `"Show Preview"` por strings `@tr` traducibles | Soporte de idioma español e internacional |
| **Media Card Texture** | `app/ui/components/media_card.slint` | Cross-fade vinculado exclusivamente a `poster-ready` | **Resuelve reciclaje de ListView:** animación determinista sin disparos en falso |
| **Grid Row Placeholder** | `app/ui/components/media_carousel.slint` | Fade-in estático de 180ms hacia fondo neutro al paginar | **Cero consumo innecesario:** sin bucles infinitos de repintado a 60/120 FPS |
| **Board Carousels** | `app/ui/pages/board.slint` | Transición fade suave en recambio de elementos de carrusel | Elimina saltos bruscos al refrescar catálogos |
| **System Tray** | `app/ui/app.slint` & `tray-icon.svg` | Icono blanco `#FFFFFF` con trazo perimetral de contraste sutil | **Soporte Dark/Light:** máxima legibilidad en temas oscuros y claros |

---

## 5. Criterios de Aceptación para la Auditoría

- [ ] La barra de búsqueda superior comparte el mismo límite horizontal izquierdo con el primer filtro (*Tipo / Película / Todo*) y la primera columna de tarjetas (`1.5 * ui-unit`).
- [ ] La fila de filtros en **Biblioteca** tiene una alineación vertical perfecta en todos sus chips (`Last Watched`, `A–Z`, etc.).
- [ ] El botón para activar la vista previa se sitúa a la derecha de la pantalla y muestra `"Mostrar vista previa"` en español con `@tr()`.
- [ ] Al hacer clic en la cruz (`x`) dentro del panel de vista previa, el panel se cierra suavemente mediante traslación/fade y el botón superior vuelve al estado de apertura, **sin producirse tirones ni recálculos continuos de columnas de la grilla**.
- [ ] El reciclaje de tarjetas en el `ListView` virtualizado no produce parpadeos ni animaciones erráticas durante el scroll rápido; el cross-fade se dispara limpiamente al cargarse la textura del póster.
- [ ] La carga de nuevas páginas utiliza un fade estático sin animaciones infinitas que mantengan activa la GPU/CPU en reposo.
- [ ] En la pantalla de inicio (*Board*), la llegada o recambio de tarjetas en *Popular* o plugins se produce con transiciones fade-in / fade-out suaves.
- [ ] El icono de la bandeja del sistema (*tray*) utiliza el símbolo de Stremio en blanco puro con trazo de contraste perimetral, siendo plenamente visible y nítido tanto en barras de tareas oscuras como claras.
- [ ] La compilación en modo release se mantiene brutalmente eficiente con consumo 0% de CPU en estado estacionario.
