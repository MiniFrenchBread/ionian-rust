#[macro_use]
extern crate tracing;

mod context;
mod controllers;
mod manager;
mod service;
mod test_util;

pub use service::{SyncMessage, SyncRequest, SyncResponse, SyncSender, SyncService};

pub struct Config {
    pub auto_sync_disabled: bool,
    pub max_sync_files: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_sync_disabled: false,
            max_sync_files: 8,
        }
    }
}

impl Config {
    pub fn disable_auto_sync(mut self) -> Self {
        self.auto_sync_disabled = true;
        self
    }
}
