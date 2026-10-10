//! The in-app owner of [`filex::settings::Settings`].
//!
//! One entity holds the settings; every mutation goes through
//! [`SettingsStore::update`], which persists off-thread and emits
//! [`SettingsEvent::Changed`] — the same event pattern as SearchInput,
//! so the workspace (and future subscribers) react uniformly without
//! polling the file.

use std::path::PathBuf;

use gpui::{Context, EventEmitter};

use filex::settings::{Settings, default_settings_file};

pub enum SettingsEvent {
    /// Previous settings snapshot; read the store for the new values.
    Changed(Settings),
}

pub struct SettingsStore {
    settings: Settings,
    /// Where saves go; `None` if the platform config dir is unknown
    /// (settings then live for the session only).
    file: Option<PathBuf>,
}

impl EventEmitter<SettingsEvent> for SettingsStore {}

impl SettingsStore {
    /// Load settings, writing first-run defaults immediately so the
    /// installer's choices are recorded once. A corrupt file logs
    /// a warning and runs on defaults; the next change overwrites it.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let file = default_settings_file();
        let mut first_run = false;
        let settings = match &file {
            Some(path) => {
                first_run = !path.exists();
                match Settings::load(path) {
                    Ok(settings) => settings,
                    Err(err) => {
                        tracing::warn!(
                            "unusable settings file ({err:#}); using defaults — the next \
                             settings change will overwrite it"
                        );
                        first_run = false;
                        Settings::default()
                    }
                }
            }
            None => {
                tracing::warn!("no config directory; settings won't persist");
                Settings::default()
            }
        };
        let store = Self { settings, file };
        if first_run {
            store.persist(cx);
        }
        store
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Apply a mutation, persist it, and notify subscribers.
    pub fn update(&mut self, cx: &mut Context<Self>, apply: impl FnOnce(&mut Settings)) {
        let previous = self.settings.clone();
        apply(&mut self.settings);
        self.persist(cx);
        cx.emit(SettingsEvent::Changed(previous));
        cx.notify();
    }

    /// Save on the background executor — settings writes never block
    /// the UI thread.
    fn persist(&self, cx: &Context<Self>) {
        let Some(file) = self.file.clone() else {
            return;
        };
        let settings = self.settings.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = settings.save(&file) {
                    tracing::error!("failed to save settings: {err:#}");
                }
            })
            .detach();
    }
}
