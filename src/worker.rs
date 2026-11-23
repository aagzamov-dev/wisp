//! Retire queued launcher work and notifications before releasing native surfaces.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

#[derive(Clone)]
pub struct WorkerLifetime {
    window: isize,
    running: Arc<AtomicBool>,
}

impl WorkerLifetime {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            window: hwnd.0 as isize,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }

    pub fn notify(&self, message: u32) {
        if self.running() {
            unsafe {
                let _ = PostMessageW(Some(HWND(self.window as _)), message, WPARAM(0), LPARAM(0));
            }
        }
    }
}
