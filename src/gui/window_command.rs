//! A command a second launch sends to the open window over the session bus,
//! which can arrive before the window exists.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use eframe::egui;

#[derive(Clone)]
pub struct Pending {
    window: Arc<OnceLock<egui::Context>>,
    /// Set when the command arrives before the window exists.
    pending: Arc<AtomicBool>,
    command: fn(&egui::Context),
}

impl Pending {
    pub fn new(command: fn(&egui::Context)) -> Self {
        Self {
            window: Arc::default(),
            pending: Arc::default(),
            command,
        }
    }

    /// Applies the command now, or once `attach` hands over the window.
    pub fn send(&self) {
        match self.window.get() {
            Some(window) => (self.command)(window),
            None => self.pending.store(true, Ordering::SeqCst),
        }
    }

    /// Called from the eframe creator: applies a command that came before the window.
    pub fn attach(&self, window: &egui::Context) {
        let _ = self.window.set(window.clone());
        if self.pending.swap(false, Ordering::SeqCst) {
            (self.command)(window);
        }
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(window: &egui::Context) {
        window.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn closes(window: &egui::Context) -> bool {
        let output = crate::egui_test::run_frame(window, [100.0, 100.0], Vec::new(), |_| {});
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|v| v.commands.contains(&egui::ViewportCommand::Close))
    }

    #[test]
    fn a_command_before_the_window_exists_is_applied_when_it_opens() {
        let pending = Pending::new(close);
        pending.send();
        assert!(pending.is_pending());

        let window = egui::Context::default();
        pending.attach(&window);
        assert!(closes(&window));
        assert!(!pending.is_pending());
    }

    #[test]
    fn a_command_after_the_window_opened_is_applied_at_once() {
        let pending = Pending::new(close);
        let window = egui::Context::default();
        pending.attach(&window);
        assert!(!closes(&window));

        pending.send();
        assert!(closes(&window));
        assert!(!pending.is_pending());
    }
}
