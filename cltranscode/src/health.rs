// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Health status for downstream connectivity.
//!
//! Used by outbox worker to pause sending when downstream is unreachable.

#![deny(unsafe_code)]

use crate::adapters::dimse_scu::DimseScu;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Downstream health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Health {
    /// Downstream is reachable - normal operation.
    Green = 0,
    /// Downstream is unreachable - pause outbox sends.
    Black = 1,
}

/// Atomic health flag shared between workers.
pub struct AtomicHealth {
    inner: AtomicU8,
}

impl AtomicHealth {
    pub fn new(initial: Health) -> Self {
        Self {
            inner: AtomicU8::new(initial as u8),
        }
    }

    pub fn load(&self) -> Health {
        match self.inner.load(Ordering::Relaxed) {
            0 => Health::Green,
            _ => Health::Black,
        }
    }

    pub fn store(&self, health: Health) {
        self.inner.store(health as u8, Ordering::Relaxed);
    }
}

/// Spawn a background C-ECHO health checker using DimseScu.
///
/// Every 5 seconds, calls `DimseScu::send_c_echo()`, updates health.
/// Initial state is Green (optimistic - let outbox try first send).
///
/// nginx-way: no hand-rolled DIMSE bytes. DimseScu already knows how to
/// associate, send commands, and parse status. C-ECHO is just C-STORE
/// without a dataset and with a different SOP Class.
pub fn spawn_health_checker(scu: Arc<Mutex<DimseScu>>, health: Arc<AtomicHealth>) {
    thread::spawn(move || {
        log::info!("health_checker: started, pinging every 5s");
        loop {
            thread::sleep(Duration::from_secs(5));
            match scu.lock().unwrap().send_c_echo() {
                Ok(()) => {
                    let old = health.load();
                    health.store(Health::Green);
                    crate::metrics::set_health_status(false);
                    if old != Health::Green {
                        log::info!("health_checker: downstream is UP");
                    }
                }
                Err(e) => {
                    let old = health.load();
                    health.store(Health::Black);
                    crate::metrics::set_health_status(true);
                    if old != Health::Green {
                        log::warn!("health_checker: downstream is DOWN: {}", e);
                    }
                }
            }
        }
    });
}
