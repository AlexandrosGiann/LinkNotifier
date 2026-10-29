@echo off
setlocal
cd /d "%~dp0"

start "" "C:\Python314\pythonw.exe" link_notifier.py --startup
