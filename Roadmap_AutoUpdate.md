# Roadmap Técnico y Especificación de Arquitectura — Sistema de Auto-Update Silencioso e In-Place (Stremio Oxide)

Este documento técnico especifica el diagnóstico exhaustivo a bajo nivel, la trazabilidad en el código fuente, la comparativa entre **Cómo Está (Estado Actual)** y **Cómo Va a Quedar (Estado Objetivo)**, y las **Restricciones Críticas de Implementación (Guardrails y Edge Cases)** para rediseñar el subsistema de actualización automática (`app/src/updater.rs` y `app/ui/app.slint`).

> [!NOTE]
> **Coordinación Multi-Agente**: Este roadmap y sus alcances (`app/src/updater.rs`, `app/ui/app.slint`, `.github/workflows/release.yml`, `Cargo.toml`) están completamente aislados de los archivos en los que está trabajando actualmente el otro agente (`downloads.rs`, `event_loop.rs`, `local_library.rs`, `discover.rs`, `library.rs`, `ratings.rs`, `core-env/src/lib.rs`).

---

## 1. Diagnóstico Profundo: ¿Por qué el Autoupdate actual falla o parece apuntar a otro lado?

### 1.1 Estado del Repositorio y Referencias Residuales
1. **Repositorio en `updater.rs`**:
   En [`app/src/updater.rs:20-21`](file:///home/kes/Documentos/Programando/stremio-native/app/src/updater.rs#L20-L21), las constantes principales ya fueron actualizadas a:
   ```rust
   const REPOSITORY_OWNER: &str = "stickyfran";
   const REPOSITORY_NAME: &str = "stremio-oxide";
   ```
   Sin embargo, aún existen metadatos heredados del upstream original (`perpetus/stremio-native`) en [`Cargo.toml:16`](file:///home/kes/Documentos/Programando/stremio-native/Cargo.toml#L16), [`app/Cargo.toml:2-4`](file:///home/kes/Documentos/Programando/stremio-native/app/Cargo.toml#L2-L4), [`app/build.rs:182`](file:///home/kes/Documentos/Programando/stremio-native/app/build.rs#L182) (`OriginalFilename = stremio-native.exe`) y en los empaquetadores de Linux en [`.github/workflows/release.yml:666-754`](file:///home/kes/Documentos/Programando/stremio-native/.github/workflows/release.yml#L666-L754).

2. **El Bug Crítico por el cual el Autoupdate NUNCA descarga ni actualiza automáticamente**:
   Aunque `REPOSITORY_OWNER` y `REPOSITORY_NAME` apuntan a `stickyfran/stremio-oxide`, **el buscador de binarios de `self_update` está roto en todas las plataformas**:
   - **En Linux y macOS** ([`app/src/updater.rs:416-419`](file:///home/kes/Documentos/Programando/stremio-native/app/src/updater.rs#L416-L419)):
     ```rust
     #[cfg(not(target_os = "windows"))]
     fn has_native_installer(_release: &Release) -> bool {
         false // <-- HARDCODEADO A FALSE EN LINUX Y MACOS
     }
     ```
     En Linux y macOS el autoupdate está **100% desactivado por código** (`can_install = false`), obligando siempre al usuario a abrir el navegador web.
   - **En Windows** ([`app/src/updater.rs:407-450`](file:///home/kes/Documentos/Programando/stremio-native/app/src/updater.rs#L407-L450)):
     `has_native_installer` llama a `release.asset_for(self_update::get_target(), Some("stremio-oxide"))`.
     En Rust, `self_update::get_target()` devuelve el target triple **`"x86_64-pc-windows-msvc"`** (o `"aarch64-pc-windows-msvc"`). `Release::asset_for` exige que el nombre del archivo en GitHub Releases contenga simultáneamente `"x86_64-pc-windows-msvc"` y `"stremio-oxide"`.
     Sin embargo, [`.github/workflows/release.yml:836-841`](file:///home/kes/Documentos/Programando/stremio-native/.github/workflows/release.yml#L836-L841) publica los archivos con estos nombres:
     - `StremioOxideSetup-v0.99.4-x64.exe`
     - `stremio-oxide-v0.99.4-windows-x64.zip`
     **Ninguno contiene el string `"x86_64-pc-windows-msvc"`**. Por lo tanto, `asset_for()` devuelve siempre `None`, `has_native_installer()` evalúa a `false`, y aún si lo encontrara, `stage_installer` busca `bin_path_in_archive("stremio-installer.exe")` que ya no existe en los releases de `stremio-oxide`.

3. **UI Invasiva Actual**:
   - En [`app/src/updater.rs:264`](file:///home/kes/Documentos/Programando/stremio-native/app/src/updater.rs#L264) y [`556-574`](file:///home/kes/Documentos/Programando/stremio-native/app/src/updater.rs#L556-L574), a los 20 segundos de abrir la app, `project_available_update` ejecuta `ui.set_update_dialog_open(true)`.
   - Esto dispara dos elementos altamente intrusivos en [`app/ui/app.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint):
     1. **Modal de pantalla completa** (`update-dialog := ModalDialog` en [`app.slint:3007-3095`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint#L3007-L3095), `z: 2000`) que bloquea toda la ventana.
     2. **Banner superior de ancho completo** ([`app.slint:1665-1702`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint#L1665-L1702), `height: 4 * root.ui-unit`, fondo `Theme.accent`) que desplaza todo el layout vertical hacia abajo.

---

## 2. Arquitectura Objetivo: Qué y Cómo Va a Quedar

### 2.1 Unificación Total del Proyecto (`stickyfran/stremio-oxide`)
1. **Fuente única de verdad**:
   - Centralizar las constantes de repositorio (`REPOSITORY_OWNER = "stickyfran"`, `REPOSITORY_NAME = "stremio-oxide"`) y asegurar que tanto `updater.rs`, `onboarding.rs`, `diagnostics.rs`, `crash_handler.rs` y `.github/workflows/release.yml` apunten de forma consistente a `https://github.com/stickyfran/stremio-oxide`.
   - Limpiar referencias residuales en `app/build.rs` y paquetes de distribución para que el binario e identidad del proyecto respondan inequívocamente a **Stremio Oxide**.

### 2.2 Reemplazo Automático del Binario desde GitHub (Multiplataforma In-Place)
En lugar de depender de instaladores externos rotos o abrir el navegador, el actualizador descargará el asset correspondiente del último Release estable de `https://github.com/stickyfran/stremio-oxide/releases` y **reemplazará el ejecutable actual en caliente (`self_replace`)**:

1. **Resolución explícita de Assets por Plataforma y Arquitectura**:
   En lugar de usar `release.asset_for(get_target())` (que falla por nomenclatura), implementar un selector explícito `select_release_asset(&Release)` que busque en `release.assets`:
   - **Linux `x86_64`**: `stremio-oxide-v{version}-x86_64-unknown-linux-gnu` (binario directo ya publicado en [`release.yml:851`](file:///home/kes/Documentos/Programando/stremio-native/.github/workflows/release.yml#L851)).
   - **Linux `aarch64`**: `stremio-oxide-v{version}-aarch64-unknown-linux-gnu` (binario directo ya publicado en [`release.yml:858`](file:///home/kes/Documentos/Programando/stremio-native/.github/workflows/release.yml#L858)).
   - **Windows `x86_64`**: `stremio-oxide-v{version}-windows-x64.zip` (extrae el binario `stremio-oxide.exe` / `stremio-native.exe` y actualiza DLLs adyacentes si aplica).
   - **Windows `aarch64`**: `stremio-oxide-v{version}-windows-arm64.zip`.
   - **macOS**: `stremio-oxide-macos.tar.gz` (extrae `stremio-oxide`).

2. **Verificación de Integridad (SHA-256)**:
   - Como `.github/workflows/release.yml:863` genera y sube `SHA256SUMS.txt` en cada release, el updater descargará opcionalmente `SHA256SUMS.txt`, calculará el hash del binario descargado en la carpeta temporal `updates/<version>/` y verificará que coincida al 100% antes de tocar el ejecutable del usuario.

3. **Reemplazo Atómico del Binario (`self_update::Move` / `self_replace`) y Reinicio**:
   - **Flujo de reemplazo**:
     1. Descargar el asset desde `asset.download_url` con `reqwest` (enviando header `Accept: application/octet-stream`) hacia un archivo temporal en el directorio de staging (`paths::get().updates()`).
     2. Si el asset es un archivo comprimido (`.zip` en Windows o `.tar.gz` en macOS), extraer el binario principal en el directorio de staging.
     3. En sistemas Unix (Linux/macOS), aplicar permisos de ejecución `chmod 0o755` (`std::os::unix::fs::PermissionsExt`).
     4. Utilizar la primitiva de reemplazo seguro de `self_update::Move::from_source(&staged_bin).replace_using_temp(&temp_file).to_dest(&current_exe)` (que en Linux/macOS hace un `rename` atómico sobre el inodo y en Windows renombra el `.exe` en ejecución a un archivo temporal antes de colocar el nuevo binario en su lugar).
     5. **Fallback de Permisos en Linux (`/usr/bin`)**: Si el usuario instaló la app mediante `.deb`/`.rpm`/`.pkg.tar.zst` en `/usr/bin/stremio-native` y el proceso actual no tiene permisos de escritura sobre `/usr/bin` (`PermissionDenied` / `EACCES`), detectar esto de antemano (`can_replace_current_exe()`):
        - Si tiene permisos de escritura (ej. binario en `~/`, `~/.local/bin`, `/opt` con permisos de usuario, o Windows en `%LOCALAPPDATA%` / carpeta portable), realiza el reemplazo automático del binario y reinicia la aplicación.
        - Si está en `/usr/bin` sin permisos de root, ofrece fallback limpio o copia en `~/.local/bin`.

---

## 3. Especificación de UI/UX: Badge Poco Invasivo Abajo a la Derecha

### 3.1 Eliminación de Elementos Intrusivos Automáticos
1. **No abrir nunca el `ModalDialog` automáticamente**:
   - En `updater.rs`, cuando `CheckKind::Automatic` detecta una actualización (`SettingsUpdateState::Available`), **no** se pondrá `ui.set_update_dialog_open(true)`.
   - El modal de notas de versión solo se abrirá si el usuario hace clic explícitamente en "Ver notas" o desde la página de Ajustes.
2. **Eliminar el Banner Superior Amarillo/Acento (`app.slint:1665-1702`)**:
   - Se elimina la barra superior de `height: 4 * root.ui-unit` que empujaba toda la interfaz hacia abajo.

### 3.2 Nuevo Componente: Floating Update Badge (Abajo a la Derecha)
Se diseñará un widget flotante compacto, elegante y no intrusivo posicionado en la esquina inferior derecha de la ventana principal (`MainWindow` en [`app/ui/app.slint`](file:///home/kes/Documentos/Programando/stremio-native/app/ui/app.slint)):

```
┌──────────────────────────────────────────────────────────────────────┐
│                                                                      │
│                         Área Principal de la App                     │
│                       (Board / Discover / Library)                   │
│                                                                      │
│                                      ┌─────────────────────────────┐ │
│                                      │ ● v0.99.5 disponible [Act.]✕│ │
│                                      └─────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

#### Propiedades de Diseño y Comportamiento del Badge:
- **Visibilidad Condicional**:
  - Solo aparece cuando `root.settings-update-state == 2` (`Available`) o `root.update-installing == true`.
  - **Oculto durante reproducción de video**: `visible: (root.settings-update-state == 2 || root.update-installing) && !root.updater-banner-dismissed && !root.effective-show-player;` para no distraer nunca mientras se reproduce una película o serie.
- **Posicionamiento Flotante (Overlay sin alterar el layout)**:
  - `x: parent.width - self.width - 1.5 * root.ui-unit;`
  - `y: parent.height - self.height - 1.5 * root.ui-unit;`
  - `z: 950;` (por encima del contenido de las páginas y carruseles, pero por debajo de modales críticos).
- **Estilo Visual (Glassmorphism / Surface Elevado)**:
  - Altura compacta: `height: 2.75 * root.ui-unit;`
  - Bordes redondeados tipo píldora/tarjeta: `border-radius: 1.375 * root.ui-unit;`
  - Fondo discreto: `background: Theme.surface-elevated;` con borde sutil `border-width: 1px; border-color: Theme.accent.with-alpha(0.45);` y `drop-shadow-blur: 16px; drop-shadow-color: #00000080;`.
- **Interacción en 1 Clic**:
  - Indicador luminoso (`8px` círculo en `Theme.accent`) + texto corto: `"v" + root.settings-update-version + " disponible"`.
  - Al hacer clic en el botón/píldora interna **"Actualizar"**:
    - Dispara inmediatamente `root.update-install()`.
    - El propio badge cambia su texto en vivo a `"Actualizando a v" + root.settings-update-version + "…"` con indicador de progreso, sin abrir ventanas emergentes que interrumpan lo que el usuario está haciendo.
    - Una vez reemplazado el binario en segundo plano, reinicia limpiamente la aplicación (o muestra `"¡Listo! Reiniciar"` para que el usuario decida el momento exacto del reinicio).
  - Botón `✕` discreto a la derecha (`updater-banner-dismissed = true`) por si el usuario desea ocultar el badge durante la sesión actual.

---

## 4. Comparativa Resumen: Cómo Está vs. Cómo Va a Quedar

| Aspecto | Cómo Está Ahora (Defecto Actual) | Cómo Va a Quedar (Estado Objetivo) |
| :--- | :--- | :--- |
| **Repositorio y Metadatos** | `updater.rs` apunta a `stickyfran/stremio-oxide`, pero hay restos de `perpetus/stremio-native` en paquetes y `build.rs`. | 100% unificado bajo `stickyfran/stremio-oxide` en todo el árbol de código y workflows. |
| **Detección de Binarios en GitHub** | Usa `release.asset_for(get_target())` buscando `"x86_64-pc-windows-msvc"`. Como los assets se llaman `...-windows-x64.zip` o `...-linux-gnu`, **falla siempre**. | Selector explícito por OS/Arch que reconoce los nombres exactos generados por `.github/workflows/release.yml`. |
| **Soporte Multiplataforma** | Linux y macOS tienen `has_native_installer() -> false` hardcodeado. Windows busca un `stremio-installer.exe` inexistente dentro del ZIP. | Reemplazo directo del binario (`self_update::Move`) en **Linux, Windows y macOS** descargando el ejecutable/archivo desde GitHub Releases. |
| **Notificación en UI** | Abre un `ModalDialog` gigante a los 20s de iniciar y muestra un banner superior que empuja toda la UI. | **Badge flotante compacto abajo a la derecha**, oculto durante el reproductor, con actualización en 1 clic y progreso in-situ. |

---

## 5. Guardrails y Casos Borde Críticos

> [!IMPORTANT]
> **1. Reemplazo de Binario en Uso (OS File Locking)**:
> - **En Linux / macOS**: El kernel permite reemplazar un binario en ejecución mediante `rename()` atómico (desvinculando el dentry anterior mientras el proceso actual mantiene su inodo abierto). Sin embargo, `fs::rename` falla con `EXDEV` (*Invalid cross-device link*) si el archivo temporal está en `/tmp` (`tmpfs`) y el binario está en `/home` o `/usr`. **Regla**: El archivo temporal para el reemplazo atómico debe crearse en el **mismo directorio padre** que `std::env::current_exe()` (p. ej., `.stremio-oxide.tmp-update`), garantizando que estén en el mismo sistema de archivos.
> - **En Windows**: Windows bloquea la escritura o eliminación de un `.exe` en ejecución, pero **sí permite renombrarlo** (`MoveFileExW`) a un nombre temporal dentro del mismo directorio (ej. `stremio-oxide.exe.old`), mover el nuevo `stremio-oxide.exe` a la ruta original, y limpiar el archivo `.old` en el siguiente arranque de la aplicación.

> [!WARNING]
> **2. Verificación de Permisos de Escritura antes de ofrecer Auto-Reemplazo**:
> Si en Linux el binario fue instalado en `/usr/bin/stremio-native` vía paquete del sistema (`apt`/`pacman`/`dnf`), el usuario normal no tiene permisos de escritura en `/usr/bin/`.
> **Regla**: La función `can_replace_current_exe()` debe comprobar que el directorio padre de `std::env::current_exe()` sea escribible por el usuario actual. Si no lo es, al pulsar el badge se abrirá o descargará el paquete correspondiente sin fallar con error críptico de permisos.

> [!TIP]
> **3. Reinicio Limpio sin Corrupción de SQLite ni Instancia Única (`single_instance`)**:
> Antes de lanzar el nuevo binario reemplazado, la aplicación debe liberar el socket/mutex de `single_instance` y cerrar el WAL de SQLite (`AppSession::shutdown`), o bien invocar el nuevo proceso mediante el mecanismo de handoff (`InstallerLauncher` reconvertido en `RestartLauncher`) justo al final de `run_application()` en [`app/src/main.rs:371`](file:///home/kes/Documentos/Programando/stremio-native/app/src/main.rs#L371), después de que `ui`, `server_task` y `db` hayan cerrado limpiamente.

---

## 6. Plan de Ejecución Paso a Paso (Aislado del otro Agente)

### Fase 1: Backend de Auto-Reemplazo de Binario (`app/src/updater.rs`)
1. Limpiar `stage_installer` y `has_native_installer` para reemplazarlos por:
   - `find_platform_asset(release: &Release) -> Option<&ReleaseAsset>` mapeando exactamente los artefactos de `stickyfran/stremio-oxide`:
     - Linux x64: `stremio-oxide-{tag}-x86_64-unknown-linux-gnu`
     - Linux arm64: `stremio-oxide-{tag}-aarch64-unknown-linux-gnu`
     - Windows x64: `stremio-oxide-{tag}-windows-x64.zip`
     - Windows arm64: `stremio-oxide-{tag}-windows-arm64.zip`
     - macOS: `stremio-oxide-macos.tar.gz`
   - `can_replace_current_exe() -> bool` que verifica permisos de escritura en el directorio del ejecutable actual y limpia cualquier binario residual `.old` de una actualización previa.
2. Implementar `download_and_replace_binary(asset: &ReleaseAsset, version: &str) -> Result<PathBuf>`:
   - Descarga el asset desde GitHub Releases al directorio `paths::get().updates()`.
   - Extrae el binario (si viene en `.zip` o `.tar.gz`) o toma el binario directo (en Linux).
   - Aplica permisos `0o755` en Unix.
   - Realiza la sustitución atómica del ejecutable actual (`self_update::Move::from_source(&staged_bin).replace_using_temp(&sibling_temp).to_dest(&current_exe)`).
   - Encola la ruta del ejecutable actualizado en el canal de handoff de cierre para que `main.rs` relance la aplicación automáticamente tras cerrar la base de datos y liberar `single_instance`.
3. Modificar `project_available_update` en `updater.rs`:
   - Eliminar `ui.set_update_dialog_open(true)` en chequeos automáticos para que solo se active el estado `SettingsUpdateState::Available` (que hace visible el badge inferior derecho).

### Fase 2: UI del Badge Flotante Abajo a la Derecha (`app/ui/app.slint`)
1. Eliminar el banner invasivo superior (`lines 1665-1702` de `app/ui/app.slint`).
2. Agregar el componente flotante **UpdateBadge** anclado abajo a la derecha (`x: parent.width - self.width - 1.5 * root.ui-unit`, `y: parent.height - self.height - 1.5 * root.ui-unit`, `z: 950`):
   - Visible únicamente cuando `(root.settings-update-state == 2 || root.update-installing) && !root.updater-banner-dismissed && !root.effective-show-player`.
   - Muestra versión disponible (`vX.Y.Z`), botón directo de acción rápida (`Actualizar` / `Actualizando…`), opción de ver notas al hacer clic sobre el texto de versión, y botón `✕` para descartar.

### Fase 3: Pruebas Unitarias y Verificación
1. Actualizar y ampliar los tests unitarios en `app/src/updater.rs` (`mod tests`) para verificar:
   - Selección correcta de assets de `stickyfran/stremio-oxide` en Linux (`x86_64`, `aarch64`), Windows (`x64`, `arm64`) y macOS.
   - Reemplazo atómico simulado sobre un binario temporal en disco.
   - No apertura automática del modal en `CheckKind::Automatic`.
