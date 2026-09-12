@echo off
setlocal enabledelayedexpansion
set "SKILL=%~dp0..\skills\fukidashi-comic-translation\SKILL.md"
if not exist "%SKILL%" set "SKILL=%~dp0skills\fukidashi-comic-translation\SKILL.md"

set "TARGET1=%USERPROFILE%\.claude\skills\fukidashi-comic-translation"
set "TARGET2=%USERPROFILE%\.codex\skills\fukidashi-comic-translation"
set "TARGET3=%USERPROFILE%\.gemini\config\skills\fukidashi-comic-translation"

for %%T in ("%TARGET1%" "%TARGET2%" "%TARGET3%") do (
    for %%P in (%%T\..) do (
        if exist "%%~fP" (
            if not exist "%%~fT" mkdir "%%~fT"
            copy /y "%SKILL%" "%%~fT\SKILL.md" >nul 2>&1
        )
    )
)
exit /b 0
