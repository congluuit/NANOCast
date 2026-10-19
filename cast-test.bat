@echo off
rem NANOCast test: tim TV tren mang (mDNS) va cast man hinh chinh 60 giay.
rem Neu khong tim thay TV, chay: cast-test.bat 192.168.x.x  (IP cua TV)
cd /d "%~dp0"
if "%~1"=="" (
    target\release\spike-win.exe --encoder 1 --discover --seconds 60 --out "%TEMP%\nanocast_test.h264"
) else (
    target\release\spike-win.exe --encoder 1 --to %1 --seconds 60 --out "%TEMP%\nanocast_test.h264"
)
echo.
pause
