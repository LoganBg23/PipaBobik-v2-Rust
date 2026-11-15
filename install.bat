@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo.
echo   PipaBobik v2 - встановлення
echo   ===========================
echo.
echo [1/4] Встановлюю mpv (програвач)...
winget install -e --id shinchiro.mpv --accept-package-agreements --accept-source-agreements
echo.
echo [2/4] Встановлюю Deno (потрібен yt-dlp для YouTube)...
winget install -e --id DenoLand.Deno --accept-package-agreements --accept-source-agreements
echo.
echo [3/4] Встановлюю yt-dlp (отримує музику з YouTube)...
winget install -e --id yt-dlp.yt-dlp --accept-package-agreements --accept-source-agreements
echo   Якщо не вийшло - не страшно: PipaBobik сам завантажить yt-dlp при першому запуску.
echo.
echo [4/4] Ярлик на робочому столі...
powershell -NoProfile -ExecutionPolicy Bypass -Command "$s=(New-Object -ComObject WScript.Shell).CreateShortcut([Environment]::GetFolderPath('Desktop')+'\pipabobik.lnk'); $s.TargetPath='%~dp0PipaBobik.exe'; $s.WorkingDirectory='%~dp0'; $s.IconLocation='%~dp0PipaBobik.exe,0'; $s.Description='pipabobik - музика з чату Twitch'; $s.Save()"
echo.
echo Готово! Відкриваю PipaBobik - там майстер налаштування.
start "" "%~dp0PipaBobik.exe"
timeout /t 5 >nul
