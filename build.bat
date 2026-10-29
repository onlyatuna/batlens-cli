@echo off
REM Build batlens (Rust). Requires: Rust toolchain (rustup) + network for crates.
cargo build --release
if errorlevel 1 exit /b %errorlevel%
copy /Y target\release\batlens.exe . >nul
echo Done: batlens.exe
