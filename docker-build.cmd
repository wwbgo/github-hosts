@echo off
rem Build and push this image to the internal registry.
rem
rem   docker-build.cmd              build + verify + push
rem   docker-build.cmd --no-push    build + verify only
rem
rem The version is NOT a parameter. Cargo.toml is the single source of truth:
rem the binary embeds that version at compile time, so an externally supplied
rem tag would name the image something the binary inside does not report. Bump
rem Cargo.toml, commit and tag, then run this.

setlocal enableextensions enabledelayedexpansion

set "REGISTRY=docker.flexem.com"
set "IMAGE=%REGISTRY%/library/github-hosts"
set "NO_PUSH="
set "PKGID="
set "VERSION="

:parse_args
if "%~1"=="" goto args_done
if /i "%~1"=="--no-push" (
    set "NO_PUSH=1"
    shift
    goto parse_args
)
if /i "%~1"=="-h"      goto usage
if /i "%~1"=="--help"  goto usage
echo [error] unknown argument: %~1
call :show_usage
endlocal
exit /b 2
:args_done

rem cargo pkgid prints e.g.  path+file:///D:/repo#1.1.0
rem or, when the name is included,  path+file:///D:/repo#github-hosts@1.1.0
for /f "usebackq delims=" %%v in (`cargo pkgid 2^>nul`) do set "PKGID=%%v"
if not defined PKGID (
    echo [error] could not read the version: `cargo pkgid` produced nothing.
    echo         Is cargo on PATH, and are you inside the crate directory?
    exit /b 1
)
set "VERSION=!PKGID:*#=!"
rem Drop a leading package name if this cargo version included one.
if not "!VERSION!"=="!VERSION:*@=!" set "VERSION=!VERSION:*@=!"
if "!VERSION!"=="" (
    echo [error] empty version after parsing "!PKGID!".
    exit /b 1
)

set "TAG=%IMAGE%:%VERSION%"

echo.
echo   image   : %TAG%
if defined NO_PUSH (
    echo   push    : skipped
) else (
    echo   push    : enabled
)
echo.

where docker >nul 2>&1
if errorlevel 1 (
    echo [error] docker is not on PATH.
    exit /b 1
)

if not defined NO_PUSH (
    if not exist "%USERPROFILE%\.docker\config.json" (
        echo [warning] no docker credential file found.
        echo           Run `docker login %REGISTRY%` first, or the push will fail.
    )
)

echo [1/3] building %TAG%
docker build --rm --build-arg VERSION=!VERSION! -f ./Dockerfile -t "%TAG%" .
if errorlevel 1 (
    echo [error] build failed.
    exit /b 1
)

rem Trust nothing: confirm the binary inside the image agrees with the tag.
echo [2/3] verifying the binary reports !VERSION!
set "REPORTED="
for /f "usebackq delims=" %%o in (`docker run --rm "%TAG%" --version`) do set "REPORTED=%%o"
if not defined REPORTED (
    echo [error] could not read a version out of the image.
    exit /b 1
)
if not "!REPORTED!"=="github-hosts !VERSION!" (
    echo [error] version mismatch: image reports "!REPORTED!", expected "github-hosts !VERSION!".
    echo         Cargo.toml and the compiled binary disagree; rebuild from a clean tree.
    exit /b 1
)
echo        ok: !REPORTED!

if defined NO_PUSH (
    echo [3/3] push skipped ^(--no-push^)
    goto done
)

echo [3/3] pushing %TAG%
docker push "%TAG%"
if errorlevel 1 (
    echo [error] push failed.
    exit /b 1
)
echo.
echo done: %TAG%

:done
endlocal
exit /b 0

:usage
call :show_usage
endlocal
exit /b 0

:show_usage
echo Usage: %~nx0 [--no-push]
echo.
echo   --no-push   build and verify only, skip the push
echo.
echo The version comes from Cargo.toml; bump it there to release a new tag.
exit /b 0
