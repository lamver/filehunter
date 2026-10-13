@echo off
chcp 65001 >nul
rem FileHunter из исходников рядом с установленной версией: своя папка данных,
rem свои вкладки и история, своё имя в сети. Установленную копию не трогает.
rem При первом запуске копирует индекс установленной версии, чтобы не сканировать диски заново.
setlocal
cd /d "%~dp0"
set "FILECAT_DATA_DIR=%LOCALAPPDATA%\FileHunter (dev)"
set "FILECAT_NAME=%COMPUTERNAME% (dev)"
set "SRC=%LOCALAPPDATA%\FileHunter"

if not exist "%FILECAT_DATA_DIR%\index.bin" if exist "%SRC%\index.bin" (
  echo Копирую индекс установленной версии...
  mkdir "%FILECAT_DATA_DIR%" 2>nul
  for %%f in (index.bin drives.json s3.json sources.json hashes.db) do if exist "%SRC%\%%f" copy /y "%SRC%\%%f" "%FILECAT_DATA_DIR%\" >nul
  if exist "%SRC%\etags" xcopy /e /i /q /y "%SRC%\etags" "%FILECAT_DATA_DIR%\etags" >nul
)

rem сборка на пониженном приоритете, чтобы компьютер не тормозил; первая - долгая
echo Собираю...
start "" /b /belownormal /wait cargo build -p filehunter
if errorlevel 1 (
  echo Сборка не удалась.
  pause
  exit /b 1
)
start "" "target\debug\filehunter.exe"
