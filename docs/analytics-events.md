<!-- Generated from src/diagnostics/analytics/catalog.rs by the
     `catalog_doc_is_current` test. Do not edit: change the catalog, then run
     FILEX_BLESS=1 cargo test --lib catalog_doc_is_current -->

# Analytics event catalog

Every product event filex can send. Types: `int`, `ms` (milliseconds), `bool`, a list of allowed labels, `id` (a fixed id from a source enum), `[…]` a list, `{…: int}` a tally, `?` optional. Design and privacy rules: [design-telemetry.md](design-telemetry.md).

## `app_launched`

The first window opened.

| Property | Type | Meaning |
|---|---|---|
| `launch` | default \| open_folder \| select_item \| this_pc \| foreign |  |
| `new_install` | bool | This run created the install ID (first launch with sharing on). |
| `startup_ms` | ms | Process start to first window. |
| `onboarding_pending` | bool | The welcome flow will run. |

## `settings_snapshot`

Settings at launch: which defaults people keep. Lists are counts.

| Property | Type | Meaning |
|---|---|---|
| `theme` | id | system, light, dark, oled. |
| `accent` | id | A preset name, or custom (the hex value is never sent). |
| `view` | id | list, grid. |
| `density` | id | comfortable, compact. |
| `grid_zoom` | int |  |
| `preview_open` | bool |  |
| `sort_by` | id | name, size, modified, kind. |
| `sort_ascending` | bool |  |
| `dirs_first` | bool |  |
| `show_hidden` | bool |  |
| `index_system` | bool |  |
| `exclude_dev` | bool |  |
| `confirm_delete` | bool |  |
| `delete_to_trash` | bool |  |
| `thumbnails` | bool |  |
| `transparency` | bool |  |
| `roots_count` | int |  |
| `favorites_count` | int |  |
| `excluded_names_count` | int |  |
| `folder_icons_count` | int |  |
| `custom_shortcuts_count` | int |  |
| `collapsed_sections_count` | int |  |

## `app_session_ended`

The process quit cleanly. Built by the pipeline.

| Property | Type | Meaning |
|---|---|---|
| `duration_s` | int |  |
| `events` | {id: int} | How many of each event this session sent. |
| `counters` | {id: int} | Shortcut use since the last summary, by action name. |
| `dropped` | {id: int} | Events dropped by the per-session cap, by name. |

## `usage_summary`

Every 30 minutes of a running session. Built by the pipeline.

| Property | Type | Meaning |
|---|---|---|
| `duration_s` | int |  |
| `since_last_summary_s` | int |  |
| `counters` | {id: int} |  |
| `dropped` | {id: int} |  |

## `resource_sample`

UI resource use, every 15 minutes.

| Property | Type | Meaning |
|---|---|---|
| `rss_mb` | int? |  |
| `index_roots` | int |  |
| `tabs` | int |  |

## `index_status`

The index daemon's state, on its first answer and after each build.

| Property | Type | Meaning |
|---|---|---|
| `trigger` | connected \| build_finished |  |
| `building` | bool |  |
| `roots` | int |  |
| `files` | int |  |
| `files_bucket` | 0 \| 1 \| 2-5 \| 6-20 \| 21-100 \| 101-1k \| 1k-10k \| 10k-100k \| 100k+ |  |
| `skipped` | int | Entries the walk couldn't read (usually permissions). |
| `error` | bool | The daemon reports an error (its text is never sent). |

## `index_unavailable`

The connection to the index daemon was lost.

| Property | Type | Meaning |
|---|---|---|
| `crash_looping` | bool |  |

## `folder_opened`

A folder's rows landed on screen.

| Property | Type | Meaning |
|---|---|---|
| `via` | launch \| up \| back \| forward \| open \| search_result \| reveal \| path_bar \| breadcrumb \| sidebar_place \| sidebar_favorite \| sidebar_drive \| recent \| this_pc \| connect_server \| magic \| picker \| new_tab \| refresh \| other |  |
| `location` | local \| home \| root \| network |  |
| `load_ms` | ms | Navigation to first paint. |
| `entries` | int |  |
| `entries_bucket` | 0 \| 1 \| 2-5 \| 6-20 \| 21-100 \| 101-1k \| 1k-10k \| 10k-100k \| 100k+ |  |

## `folder_load_failed`

A folder failed to load.

| Property | Type | Meaning |
|---|---|---|
| `via` | launch \| up \| back \| forward \| open \| search_result \| reveal \| path_bar \| breadcrumb \| sidebar_place \| sidebar_favorite \| sidebar_drive \| recent \| this_pc \| connect_server \| magic \| picker \| new_tab \| refresh \| other |  |
| `location` | local \| home \| root \| network |  |
| `load_ms` | ms |  |
| `error` | not_found \| permission_denied \| already_exists \| timed_out \| interrupted \| invalid \| storage_full \| read_only \| directory_not_empty \| not_a_directory \| is_a_directory \| crosses_devices \| busy \| unsupported \| network \| canceled \| io_other \| other |  |

## `search_session`

One search, first keystroke to open, clear or apply. The query text is never sent.

| Property | Type | Meaning |
|---|---|---|
| `outcome` | opened \| revealed \| cleared \| magic_applied \| magic_command \| magic_go |  |
| `rank` | int? | Row index of the opened result. |
| `duration_ms` | ms |  |
| `edits` | int |  |
| `max_query_len` | int |  |
| `final_query_len` | int |  |
| `responses` | int |  |
| `first_response_ms` | int? |  |
| `mean_response_ms` | int? |  |
| `max_response_ms` | int |  |
| `results` | int | Rows in the last response. |
| `results_bucket` | 0 \| 1 \| 2-5 \| 6-20 \| 21-100 \| 101-1k \| 1k-10k \| 10k-100k \| 100k+ |  |
| `zero_result_responses` | int |  |
| `errors` | int |  |
| `pages_loaded` | int |  |
| `filters` | [tag \| kind \| ext \| size \| modified] |  |
| `phrases` | bool | Natural-language phrases became filters. |
| `magic` | bool |  |
| `scope` | anywhere \| current_dir |  |
| `hit_limit` | bool | The daemon's candidate budget was hit. |
| `literal_won` | bool |  |

## `magic_plan_shown`

A Magic plan was shown (once per search).

| Property | Type | Meaning |
|---|---|---|
| `verb` | delete \| move \| copy \| rename \| compress \| tag \| untag \| organize |  |
| `matches` | int |  |
| `ok` | bool |  |
| `refusal` | id? | `PlanError::id` when refused. |

## `magic_plan_applied`

A Magic plan was confirmed.

| Property | Type | Meaning |
|---|---|---|
| `verb` | delete \| move \| copy \| rename \| compress \| tag \| untag \| organize |  |
| `selected` | int |  |
| `unchecked` | int |  |
| `local` | bool |  |
| `conflicts` | keep_both \| skip \| replace |  |

## `magic_instant`

A Magic instant command ran.

| Property | Type | Meaning |
|---|---|---|
| `kind` | app \| go |  |
| `command` | id? | `AppCommand::id` for app commands. |
| `new_tab` | bool? |  |

## `magic_mode_toggled`

The Magic toggle in the search bar.

| Property | Type | Meaning |
|---|---|---|
| `on` | bool |  |

## `search_scope_changed`

The search scope menu.

| Property | Type | Meaning |
|---|---|---|
| `scope` | anywhere \| current_dir |  |

## `file_op`

A file operation batch finished (or a single op failed).

| Property | Type | Meaning |
|---|---|---|
| `op` | move \| copy \| rename \| trash \| create \| mixed \| none |  |
| `source` | paste \| drop \| delete \| magic \| single \| new_folder \| compress |  |
| `ok` | bool |  |
| `items` | int |  |
| `requested` | int |  |
| `kinds` | {move \| copy \| rename \| trash \| create \| mixed \| none: int} |  |
| `duration_ms` | ms? |  |
| `error` | not_found \| permission_denied \| already_exists \| timed_out \| interrupted \| invalid \| storage_full \| read_only \| directory_not_empty \| not_a_directory \| is_a_directory \| crosses_devices \| busy \| unsupported \| network \| canceled \| io_other \| other? |  |

## `undo`

An undo finished.

| Property | Type | Meaning |
|---|---|---|
| `items` | int |  |
| `kinds` | {move \| copy \| rename \| trash \| create \| mixed \| none: int} |  |
| `ok` | bool |  |
| `error` | not_found \| permission_denied \| already_exists \| timed_out \| interrupted \| invalid \| storage_full \| read_only \| directory_not_empty \| not_a_directory \| is_a_directory \| crosses_devices \| busy \| unsupported \| network \| canceled \| io_other \| other? |  |

## `file_opened`

A file was opened in an app.

| Property | Type | Meaning |
|---|---|---|
| `kind` | directory \| image \| video \| audio \| archive \| code \| document \| other |  |
| `source` | browse \| search \| open_with |  |
| `app` | default \| chosen |  |
| `ok` | bool? | Known only for the default app (the chosen app opens off-thread). |

## `tag_saved`

A tag was saved in the tag editor (its name is never sent).

| Property | Type | Meaning |
|---|---|---|
| `edited_existing` | bool |  |

## `context_menu_used`

A context menu command.

| Property | Type | Meaning |
|---|---|---|
| `command` | id | `context_menu::Command::id`. |
| `target` | entry \| search_result \| favorite \| folder_icon \| network_drive \| background |  |
| `selected` | int |  |

## `toolbar_button_clicked`

A top-bar navigation button was clicked.

| Property | Type | Meaning |
|---|---|---|
| `action` | back \| forward \| parent \| refresh |  |

## `tab_changed`

A tab was opened or closed.

| Property | Type | Meaning |
|---|---|---|
| `action` | opened \| closed |  |
| `tabs` | int | Tabs open afterwards. |

## `feature_used`

A small affordance was used.

| Property | Type | Meaning |
|---|---|---|
| `feature` | quick_look \| drag_out \| settings_opened \| sidebar_tag_search \| tag_removed \| analytics_enabled |  |
| `items` | int? | Items involved, where it applies. |

## `setting_changed`

A setting changed.

| Property | Type | Meaning |
|---|---|---|
| `setting` | theme \| accent \| view \| density \| sort_by \| sort_ascending \| dirs_first \| grid_zoom \| preview_open \| show_hidden \| index_system \| exclude_dev \| confirm_delete \| delete_to_trash \| thumbnails \| transparency \| roots \| favorites \| excluded_names \| folder_icons \| custom_shortcuts \| collapsed_sections |  |
| `value` | id \| bool \| int (list length) |  |

## `onboarding_step`

A move between onboarding screens.

| Property | Type | Meaning |
|---|---|---|
| `from` | loading \| welcome \| theme \| favorites \| search \| magic \| default_app \| ready |  |
| `to` | loading \| welcome \| theme \| favorites \| search \| magic \| default_app \| ready |  |
| `on_step_ms` | ms | Time spent on the screen being left. |

## `onboarding_finished`

The onboarding flow closed.

| Property | Type | Meaning |
|---|---|---|
| `completed` | bool | Finished rather than skipped. |
| `last_step` | loading \| welcome \| theme \| favorites \| search \| magic \| default_app \| ready |  |
| `imported_favorites` | int |  |
| `offered_favorites` | int |  |
| `default_app` | bool |  |
| `magic_live` | bool |  |

## `update`

A step of the update flow (the offered version isn't sent).

| Property | Type | Meaning |
|---|---|---|
| `stage` | available \| copy_command \| open_url \| restart \| dismissed \| failed |  |

## Events outside the typed catalog

- `$exception`: a panic (drained from the crash queue) or an `error!` log, in PostHog's exception format. Text passes `exception::scrub_text`. See design-telemetry.md.
- `feedback_submitted`: feedback the user typed and sent: `message` (scrubbed of paths, file names, emails and URLs; at most 5000 chars) and `length`.
