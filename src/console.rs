// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Evan Chen <evanchen940628@gmail.com>. All rights reserved.

//! Console state and terminal size helpers for the CLI.

use batlens_core::sys::HandleGuard;
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP,
};

static RESTORE_CP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static RESTORE_MODE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CONSOLE_RESTORED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn restore_console_state() {
    // Only one caller (Ctrl handler or ConsoleGuard::drop) restores the console.
    if CONSOLE_RESTORED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let mode_packed = RESTORE_MODE.swap(0, std::sync::atomic::Ordering::SeqCst);
    if mode_packed != 0 {
        let mode = mode_packed as u32;
        let h_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if h_out != INVALID_HANDLE_VALUE && !h_out.is_null() {
            unsafe { SetConsoleMode(h_out, mode) };
        }
    }
    let cp = RESTORE_CP.swap(0, std::sync::atomic::Ordering::SeqCst);
    if cp != 0 && cp != 65001 {
        unsafe { SetConsoleOutputCP(cp) };
    }
}

unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> windows_sys::core::BOOL {
    restore_console_state();
    0
}
pub struct ConsoleGuard;

impl ConsoleGuard {
    pub fn init() -> Self {
        CONSOLE_RESTORED.store(false, std::sync::atomic::Ordering::SeqCst);
        let original_cp = unsafe { GetConsoleOutputCP() };
        if original_cp != 0 && original_cp != 65001 {
            unsafe { SetConsoleOutputCP(65001) };
            RESTORE_CP.store(original_cp, std::sync::atomic::Ordering::SeqCst);
        }

        let h_out = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if h_out != INVALID_HANDLE_VALUE && !h_out.is_null() {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h_out, &mut mode) } != 0 {
                let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                if unsafe { SetConsoleMode(h_out, new_mode) } != 0 {
                    RESTORE_MODE.store(
                        mode as u64 | 0x1_0000_0000,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                } else {
                    colored::control::set_override(false);
                }
            } else {
                colored::control::set_override(false);
            }
        } else {
            colored::control::set_override(false);
        }

        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
        }

        Self
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 0);
        }
        restore_console_state();
    }
}

pub fn chart_width_from_cols(cols: usize) -> usize {
    let available = cols.saturating_sub(9);
    let min_bound = 30.min(available);
    available.clamp(min_bound, 300)
}

pub fn terminal_columns() -> usize {
    let conout: [u16; 8] = [67, 79, 78, 79, 85, 84, 36, 0]; // "CONOUT$\0"
    let handle = unsafe {
        CreateFileW(
            conout.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0 as _,
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        let _guard = HandleGuard(handle);
        let mut csbi: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetConsoleScreenBufferInfo(handle, &mut csbi) != 0 };
        if ok {
            let cols = (csbi.srWindow.Right - csbi.srWindow.Left + 1).max(0) as usize;
            if cols > 0 {
                return cols;
            }
        }
    }

    if let Ok(cols_str) = std::env::var("COLUMNS")
        && let Ok(cols) = cols_str.trim().parse::<usize>()
        && cols > 0
    {
        return cols;
    }

    80
}

pub fn default_chart_width() -> usize {
    chart_width_from_cols(terminal_columns())
}
