//! Keeps the desktop from logging out or suspending while an import runs.
//!
//! A laptop that suspends (idle timer, a shut lid, a flat battery warning)
//! or logs out halfway through copying a memory card is how a day of photos
//! is lost. While an [`ImportGuard`] exists, the session manager asks the
//! user before logging out and doesn't suspend on its own; dropping the
//! guard (the import finished or stopped) lifts that.

use gtk4::prelude::*;

pub struct ImportGuard {
    app: gtk4::Application,
    cookie: u32,
}

impl ImportGuard {
    /// Hold off logout and suspend, saying why (the desktop shows `reason`
    /// in its "Photon is preventing…" prompt). None when the desktop has no
    /// session manager to ask: the import still runs, unprotected.
    pub fn new(app: &gtk4::Application, window: &impl IsA<gtk4::Window>, reason: &str) -> Option<Self> {
        let cookie = app.inhibit(
            Some(window),
            gtk4::ApplicationInhibitFlags::LOGOUT | gtk4::ApplicationInhibitFlags::SUSPEND,
            Some(reason),
        );
        if cookie == 0 {
            log::warn!("The desktop can't hold off logout and suspend during the import");
            return None;
        }
        Some(Self { app: app.clone(), cookie })
    }
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        self.app.uninhibit(self.cookie);
    }
}
