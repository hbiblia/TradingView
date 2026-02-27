@echo off
echo Compilando tradingbot (release)...
cargo build --release
if %ERRORLEVEL% neq 0 (
    echo.
    echo ERROR: La compilacion fallo.
    pause
    exit /b 1
)
copy /y target\release\tradingbot.exe bin\tradingbot.exe >nul
echo.
echo Listo! Binario actualizado: tradingbot.exe
