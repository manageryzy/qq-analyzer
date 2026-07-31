@echo off
setlocal

set "WORKSPACE_ROOT=%~dp0..\.."
pushd "%WORKSPACE_ROOT%" || exit /b 1

set "CARGO_HOME=%CD%\qq-analyzer\output\_deps\cargo-home-windows"
set "CFLAGS=/MD"
set "CXXFLAGS=/MD"

cargo %*
set "STATUS=%ERRORLEVEL%"

popd
exit /b %STATUS%
