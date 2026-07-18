//! The in-app owner of [`filex::settings::Settings`].
//!
//! One entity holds the settings; every mutation goes through
//! [`SettingsStore::update`], which persists off-thread and emits
//! [`SettingsEvent::Changed`] — the same event pattern as SearchInput,
//! so the workspace (and future subscribers) react uniformly without
//! polling the file.

use std::path::PathBuf;

use gpui::{Context, EventEmitter};

use filex::index::manager;
use filex::settings::{Settings, default_settings_file};

pub enum SettingsEvent {
    /// Some setting changed; read the store for the new values.
    Changed,
}

pub struct SettingsStore {
    settings: Settings,
    /// Where saves go; `None` if the platform config dir is unknown
    /// (settings then live for the session only).
    file: Option<PathBuf>,
}

impl EventEmitter<SettingsEvent> for SettingsStore {}

impl SettingsStore {
    /// Load settings (migrating roots from the legacy roots.list on
    /// first launch — the migrated file is written immediately so the
    /// settings file becomes the source of truth). A corrupt file logs
    /// a warning and runs on defaults; the next change overwrites it.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let file = default_settings_file();
        let legacy = manager::default_roots_file();
        let mut migrated = false;
        let settings = match &file {
            Some(path) => {
                migrated = !path.exists();
                match Settings::load(path, legacy.as_deref()) {
                    Ok(settings) => settings,
                    Err(err) => {
                        tracing::warn!(
                            "unusable settings file ({err:#}); using defaults — the next \
                             settings change will overwrite it"
                        );
                        migrated = false;
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
        if migrated {
            store.persist(cx);
        }
        store
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Apply a mutation, persist it, and notify subscribers.
    pub fn update(&mut self, cx: &mut Context<Self>, apply: impl FnOnce(&mut Settings)) {
        apply(&mut self.settings);
        self.persist(cx);
        cx.emit(SettingsEvent::Changed);
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
