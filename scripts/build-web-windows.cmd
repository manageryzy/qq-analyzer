@echo off
setlocal
set "SCRIPT_DIR=%~dp0"
set "REPO_DIR=%SCRIPT_DIR%.."

pushd "%REPO_DIR%\web" || exit /b 1
call npm ci || (popd & exit /b 1)
call npm run check || (popd & exit /b 1)
call npm test || (popd & exit /b 1)
call npm run build || (popd & exit /b 1)
popd

call "%SCRIPT_DIR%cargo-msvc.cmd" build ^
  --manifest-path "%REPO_DIR%\rust-msg3-parser\Cargo.toml" ^
  --release ^
  --features web-ui,image-index,image-index-qdrant,image-index-clip-cuda,image-index-clip-tensorrt,image-index-sscd-cuda,image-index-sscd-tensorrt ^
  --bin qq_analyzer_rs
exit /b %ERRORLEVEL%
