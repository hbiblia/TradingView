@echo off
echo Compilando trading-view (release)...
cargo build --release
if %ERRORLEVEL% neq 0 (
    echo.
    echo ERROR: La compilacion fallo.
    pause
    exit /b 1
)
copy /y target\release\trading-view.exe bin\trading-view.exe >nul
echo.
echo Listo! Binario actualizado: trading-view.exe
