@echo off
setlocal

set "STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "STARTFILE=%STARTUP%\LinkNotifier.cmd"

if exist "%STARTFILE%" (
    del "%STARTFILE%"
    echo Link Notifier was removed from Windows Startup.
) else (
    echo Link Notifier was not found in Windows Startup.
)

pause
