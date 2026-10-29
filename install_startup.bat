@echo off
setlocal
cd /d "%~dp0"

set "PYTHONW=C:\Python314\pythonw.exe"
set "STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "STARTFILE=%STARTUP%\LinkNotifier.cmd"

if not exist "%PYTHONW%" (
    echo ERROR: pythonw.exe was not found at:
    echo %PYTHONW%
    echo.
    echo Edit install_startup.bat if Python is installed elsewhere.
    pause
    exit /b 1
)

echo @echo off>"%STARTFILE%"
echo start "" "%PYTHONW%" "%CD%\link_notifier.py" --startup>>"%STARTFILE%"

echo.
echo SUCCESS: Link Notifier was added to Windows Startup.
echo.
echo Startup file:
echo %STARTFILE%
echo.
echo It will launch automatically the next time you sign in.
pause
