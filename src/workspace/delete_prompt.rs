//! The delete confirmation: one prompt for moving items to the Recycle
//! Bin / Trash and for deleting them permanently, worded the way the
//! platform's file manager words it.

use super::*;

/// The platform's name for its trash.
pub(super) const TRASH_NAME: &str = if cfg!(windows) {
    "Recycle Bin"
} else {
    "Trash"
};

/// An open delete prompt: what it deletes, and how.
pub(super) struct DeletePrompt {
    pub paths: Vec<PathBuf>,
    /// "“name”" or "N items" (see `describe_items`).
    pub label: String,
    pub permanent: bool,
}

/// Whether a delete asks first: a permanent delete always does; moving to
/// the trash does while "Confirm before deleting" is on.
pub(super) fn needs_prompt(permanent: bool, confirm_setting: bool) -> bool {
    permanent || confirm_setting
}

/// What the prompt says.
#[derive(Debug, PartialEq)]
pub(super) struct PromptCopy {
    pub title: String,
    pub message: String,
    pub button: String,
}

/// The prompt's words for `label` ("“name”" or "N items").
pub(super) fn prompt_copy(label: &str, count: usize, permanent: bool) -> PromptCopy {
    let pronoun = if count == 1 { "it" } else { "them" };
    if permanent {
        let subject = if count == 1 { "It won't" } else { "They won't" };
        PromptCopy {
            title: format!("Permanently delete {label}?"),
            message: format!("{subject} go to the {TRASH_NAME}, and this can't be undone."),
            button: "Delete".to_owned(),
        }
    } else {
        PromptCopy {
            title: format!("Move {label} to the {TRASH_NAME}?"),
            message: format!("You can restore {pronoun} from the {TRASH_NAME}."),
            button: format!("Move to {TRASH_NAME}"),
        }
    }
}

impl Workspace {
    /// Delete `paths`: ask first when [`needs_prompt`] says so, otherwise
    /// move them to the trash straight away.
    pub(super) fn request_delete(
        &mut self,
        paths: Vec<PathBuf>,
        label: String,
        permanent: bool,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let confirm = self.settings.read(cx).settings().confirm_delete;
        if needs_prompt(permanent, confirm) {
            self.delete_prompt = Some(DeletePrompt {
                paths,
                label,
                permanent,
            });
            cx.notify();
        } else {
            self.delete_paths(paths, cx);
        }
    }

    /// Answer the open prompt: go ahead, or cancel.
    pub(super) fn resolve_delete(&mut self, confirm: bool, cx: &mut Context<Self>) {
        let Some(prompt) = self.delete_prompt.take() else {
            return;
        };
        cx.notify();
        if !confirm {
            return;
        }
        if prompt.permanent {
            self.delete_paths_permanently(prompt.paths, prompt.label, cx);
        } else {
            self.delete_paths(prompt.paths, cx);
        }
    }

    /// Delete every path for good, off the UI thread. Stops at the first
    /// failure and reports it; there is no undo.
    fn delete_paths_permanently(
        &mut self,
        paths: Vec<PathBuf>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if !self.validate_search_paths(&paths, cx) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let failure = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .iter()
                        .find_map(|path| ops::delete_permanently(path).err())
                        .map(|err| format!("{err:#}"))
                })
                .await;
            this.update(cx, |this, cx| {
                this.notice = Some(
                    failure
                        .unwrap_or_else(|| format!("permanently deleted {label}"))
                        .into(),
                );
                this.refresh_after_op(cx);
            })
            .ok();
        })
        .detach();
    }

    /// The open prompt as a dialog, or `None`. Enter confirms and Escape
    /// cancels (the OpenSelected and ClearInput handlers).
    pub(super) fn render_delete_modal(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let prompt = self.delete_prompt.as_ref()?;
        let theme = *cx.theme();
        let copy = prompt_copy(&prompt.label, prompt.paths.len(), prompt.permanent);
        let (tone, variant) = if prompt.permanent {
            (ui::modal::Tone::Danger, ui::button::Variant::Danger)
        } else {
            (ui::modal::Tone::Neutral, ui::button::Variant::Primary)
        };
        Some(ui::modal::present(
            "delete-backdrop-in",
            ui::modal::backdrop(&theme, "delete-backdrop")
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.resolve_delete(false, cx);
                }))
                .child(
                    ui::modal::panel(&theme, "delete-panel")
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(ui::modal::intro(
                            ui::modal::glyph(&theme, "icons/trash-2.svg", tone),
                            ui::modal::title(&theme, copy.title),
                            ui::modal::message(&theme, copy.message),
                        ))
                        .child(
                            ui::modal::buttons(&theme)
                                .child(
                                    ui::modal::button(
                                        &theme,
                                        "delete-cancel",
                                        "Cancel",
                                        ui::button::Variant::Secondary,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _window, cx| {
                                            this.resolve_delete(false, cx);
                                        },
                                    )),
                                )
                                .child(
                                    ui::modal::button(
                                        &theme,
                                        "delete-confirm",
                                        copy.button,
                                        variant,
                                    )
                                    .on_click(cx.listener(
                                        |this, _: &ClickEvent, _window, cx| {
                                            this.resolve_delete(true, cx);
                                        },
                                    )),
                                ),
                        ),
                ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_deletes_always_ask() {
        assert!(needs_prompt(true, false));
        assert!(needs_prompt(true, true));
        assert!(needs_prompt(false, true));
        assert!(!needs_prompt(false, false));
    }

    #[test]
    fn trash_prompt_names_the_platform_bin() {
        let one = prompt_copy("“a.txt”", 1, false);
        assert_eq!(one.title, format!("Move “a.txt” to the {TRASH_NAME}?"));
        assert_eq!(
            one.message,
            format!("You can restore it from the {TRASH_NAME}.")
        );
        assert_eq!(one.button, format!("Move to {TRASH_NAME}"));
        let many = prompt_copy("3 items", 3, false);
        assert!(many.message.contains("restore them"));
        #[cfg(windows)]
        assert_eq!(TRASH_NAME, "Recycle Bin");
    }

    #[test]
    fn permanent_prompt_says_it_cannot_be_undone() {
        let one = prompt_copy("“a.txt”", 1, true);
        assert_eq!(one.title, "Permanently delete “a.txt”?");
        assert!(one.message.starts_with("It won't") && one.message.contains("can't be undone"));
        assert_eq!(one.button, "Delete");
        assert!(
            prompt_copy("2 items", 2, true)
                .message
                .starts_with("They won't")
        );
    }
}
