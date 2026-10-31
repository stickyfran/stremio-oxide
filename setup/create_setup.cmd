@echo off
setlocal

cd /d "%~dp0.."

if not defined APP_VERSION (
  for /f "tokens=3" %%V in ('findstr /b /c:"version = " app\Cargo.toml') do set "APP_VERSION=%%~V"
)

if not defined APP_VERSION (
  echo Unable to determine the application version from app\Cargo.toml.
  exit /b 1
)

if not defined ISCC_EXE set "ISCC_EXE=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe"
if not exist "%ISCC_EXE%" set "ISCC_EXE=%ProgramFiles%\Inno Setup 6\ISCC.exe"

if not exist "%ISCC_EXE%" (
  echo Inno Setup 6 was not found. Set ISCC_EXE to the full path of ISCC.exe.
  exit /b 1
)

echo Building Stremio Oxide %APP_VERSION%...
if not defined SKIP_BUILD (
  cargo build --release --package stremio-native
  if errorlevel 1 exit /b %errorlevel%
) else (
  echo Reusing the completed Cargo release build.
)

if not defined TARGET_ARCH set "TARGET_ARCH=x64"
if not defined BUILD_ROOT set "BUILD_ROOT=%CD%\target\release"

if exist "%BUILD_ROOT%\stremio-native.exe" (
  copy /y "%BUILD_ROOT%\stremio-native.exe" "%BUILD_ROOT%\stremio-oxide.exe" >nul
)

echo Staging the app-local MSVC runtime for %TARGET_ARCH%...
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\stage_windows_msvc_runtime.ps1 -BuildRoot "%BUILD_ROOT%" -Architecture "%TARGET_ARCH%"
if errorlevel 1 exit /b %errorlevel%

if /i "%TARGET_ARCH%"=="arm64" (
  set "OUTPUT_NAME=StremioOxideSetup-v%APP_VERSION%-arm64"
  set "ARCH_ALLOWED=arm64"
  set "PORTABLE_ARCHIVE=%CD%\artifacts\stremio-oxide-v%APP_VERSION%-windows-arm64.zip"
) else (
  set "OUTPUT_NAME=StremioOxideSetup-v%APP_VERSION%-x64"
  set "ARCH_ALLOWED=x64compatible"
  set "PORTABLE_ARCHIVE=%CD%\artifacts\stremio-oxide-v%APP_VERSION%-windows-x64.zip"
)

echo Creating the installer...
"%ISCC_EXE%" "/DAppVersion=%APP_VERSION%" "/DBuildRoot=%BUILD_ROOT%" "/DArtifactsDir=%CD%\artifacts" "/DOutputBaseName=%OUTPUT_NAME%" "/DArchAllowed=%ARCH_ALLOWED%" setup\StremioNative.iss
if errorlevel 1 exit /b %errorlevel%

set "INSTALLER=%CD%\artifacts\%OUTPUT_NAME%.exe"
if not exist "%INSTALLER%" (
  echo Expected installer was not created: %INSTALLER%
  exit /b 1
)

echo Creating the portable application archive...
set "PORTABLE_STAGE_DIR=%CD%\artifacts\stremio-oxide-portable"
if exist "%PORTABLE_STAGE_DIR%" rmdir /s /q "%PORTABLE_STAGE_DIR%"
mkdir "%PORTABLE_STAGE_DIR%"
if exist "%BUILD_ROOT%\stremio-oxide.exe" copy /y "%BUILD_ROOT%\stremio-oxide.exe" "%PORTABLE_STAGE_DIR%\" >nul
if exist "%BUILD_ROOT%\stremio-native.exe" copy /y "%BUILD_ROOT%\stremio-native.exe" "%PORTABLE_STAGE_DIR%\" >nul
if exist "%BUILD_ROOT%\libmpv-2.dll" copy /y "%BUILD_ROOT%\libmpv-2.dll" "%PORTABLE_STAGE_DIR%\" >nul

if exist "%BUILD_ROOT%\msvc-runtime" (
  mkdir "%PORTABLE_STAGE_DIR%\msvc-runtime"
  xcopy /y /e "%BUILD_ROOT%\msvc-runtime" "%PORTABLE_STAGE_DIR%\msvc-runtime\" >nul
)

if exist "%BUILD_ROOT%\licenses" (
  mkdir "%PORTABLE_STAGE_DIR%\licenses"
  xcopy /y /e "%BUILD_ROOT%\licenses" "%PORTABLE_STAGE_DIR%\licenses\" >nul
)

if not exist "%CD%\artifacts" mkdir "%CD%\artifacts"
if exist "%PORTABLE_ARCHIVE%" del /q "%PORTABLE_ARCHIVE%"
tar -a -c -f "%PORTABLE_ARCHIVE%" -C "%PORTABLE_STAGE_DIR%" .
if errorlevel 1 exit /b %errorlevel%

rmdir /s /q "%PORTABLE_STAGE_DIR%"

echo Installer and portable archive created in %CD%\artifacts.
