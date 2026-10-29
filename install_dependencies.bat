@echo off
setlocal

set "PYTHON=C:\Python314\python.exe"

if not exist "%PYTHON%" (
    echo ERROR: Python was not found at:
    echo %PYTHON%
    pause
    exit /b 1
)

"%PYTHON%" -m pip install --user winotify

echo.
echo Dependencies installed.
pause
