@echo off
title AI Gateway Backend
cd /d "E:\ai_api_free"

echo ========================================================
echo   AI GATEWAY - KHOI DONG BACKEND
echo   Thu muc lam viec: E:\ai_api_free
echo   Dia chi API:     http://127.0.0.1:8080
echo   Web Dashboard:   http://127.0.0.1:8080/
echo ========================================================
echo.

:: Giai phong tien trinh cu neu dang chay ngam
taskkill /F /IM ai-gateway.exe >nul 2>&1

if exist "target\release\ai-gateway.exe" (
    echo [OK] Dang chay ban Release toi uu...
    "target\release\ai-gateway.exe"
) else if exist "target\debug\ai-gateway.exe" (
    echo [OK] Dang chay ban Debug...
    "target\debug\ai-gateway.exe"
) else (
    echo [!] Khong tim thay file thuc thi, dang bien dich...
    "C:\Users\admin\scoop\persist\rustup\.cargo\bin\cargo.exe" build --release
    if exist "target\release\ai-gateway.exe" (
        "target\release\ai-gateway.exe"
    ) else (
        echo [LOI] Khong the bien dich ai-gateway.
        pause
    )
)
