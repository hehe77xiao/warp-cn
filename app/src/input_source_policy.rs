//! macOS-only policy: manage the keyboard input source on terminal pane activation.
//!
//! - Exempt panes (title matches the exemption rule): always restore that pane's
//!   remembered input source (snapshot taken when the pane loses focus).
//! - Non-exempt panes: force the English keyboard layout. Each forced TIS switch
//!   carries a synchronous main-thread cost (IME host XPC plus AppKit's follow-up
//!   IMK session activation and menu rebuild), which the debouncing below and the
//!   platform-side handle caching aim to minimize rather than repeat.
//! - Requests are debounced: only the last requested target within the settle
//!   window is applied, so rapid pane ping-pongs collapse into a single
//!   (possibly no-op) switch.

use std::collections::HashMap;
use std::time::Duration;

use settings::macros::implement_setting_for_enum;
use settings::{Setting, SupportedPlatforms, SyncToCloud};
use warpui::r#async::Timer;
use warpui::{AppContext, Entity, SingletonEntity, ViewContext, ViewHandle, WindowId};

use crate::pane_group::PaneGroup;
use crate::pane_group::pane::{PaneConfiguration, PaneId};
use crate::terminal::TerminalView;
use crate::terminal::general_settings::{GeneralSettings, GeneralSettingsChangedEvent};
use crate::workspace::view::Workspace;

/// Built-in English keyboard layout selected whenever the exemption rule doesn't
/// match. Not user-configurable on purpose: "English" must not be an IME that
/// could land in a composing state.
pub const ENGLISH_INPUT_SOURCE_ID: &str = "com.apple.keylayout.ABC";

/// Bounds the per-pane memory so long-lived sessions with churn don't grow it
/// without limit.
const MAX_REMEMBERED_PANES: usize = 512;

/// A requested input-source switch is applied once focus activity has been
/// quiet for this long, collapsing rapid pane switches.
const SWITCH_SETTLE: Duration = Duration::from_millis(300);

#[derive(
    Default,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    PartialEq,
    Copy,
    Clone,
    schemars::JsonSchema,
    settings_value::SettingsValue,
)]
#[schemars(
    description = "How a pane title is matched against the input-method exemption pattern.",
    rename_all = "snake_case"
)]
pub enum PaneTitleMatchMode {
    #[default]
    StartsWith,
    NotStartsWith,
    Contains,
    NotContains,
}

implement_setting_for_enum!(
    PaneTitleMatchMode,
    GeneralSettings,
    SupportedPlatforms::MAC,
    SyncToCloud::Never,
    surface: settings::SettingSurfaces::GUI,
    private: false,
    toml_path: "general.pane_title_match_mode",
    description: "How a pane title is matched against the input-method exemption pattern.",
);

/// Whether the pane title matches the configured exemption rule. Matching is
/// case-sensitive.
#[must_use]
pub fn title_is_exempt(title: &str, mode: PaneTitleMatchMode, pattern: &str) -> bool {
    match mode {
        PaneTitleMatchMode::StartsWith => title.starts_with(pattern),
        PaneTitleMatchMode::NotStartsWith => !title.starts_with(pattern),
        PaneTitleMatchMode::Contains => title.contains(pattern),
        PaneTitleMatchMode::NotContains => !title.contains(pattern),
    }
}

/// A debounced input-source switch waiting to be applied. The generation is
/// captured by the settle timer so stale timers discard instead of applying an
/// outdated target.
struct PendingSwitch {
    target: String,
    trigger: &'static str,
    seq: u64,
}

/// Per-pane input source memory: the source each pane was using when it last
/// lost focus, plus the last focused pane per window so we know which pane to
/// snapshot when focus moves.
#[derive(Default)]
pub struct InputSourceMemoryModel {
    remembered: HashMap<PaneId, String>,
    last_focused: HashMap<WindowId, PaneId>,
    pending: Option<PendingSwitch>,
    switch_seq: u64,
}

impl Entity for InputSourceMemoryModel {
    type Event = ();
}

impl SingletonEntity for InputSourceMemoryModel {}

impl InputSourceMemoryModel {
    pub fn new() -> Self {
        Self::default()
    }

    fn remember(&mut self, pane_id: PaneId, source_id: &str) {
        if self.remembered.len() >= MAX_REMEMBERED_PANES {
            self.remembered.clear();
        }
        self.remembered.insert(pane_id, source_id.to_owned());
    }
}

fn policy_enabled(ctx: &AppContext) -> bool {
    *GeneralSettings::as_ref(ctx).switch_input_method_on_pane_activation
}

fn current_input_source_id(ctx: &AppContext) -> Option<String> {
    ctx.current_input_source_id()
}

fn remember_source(ctx: &mut AppContext, pane_id: PaneId, source_id: &str) {
    InputSourceMemoryModel::handle(ctx).update(ctx, |model, _| {
        model.remember(pane_id, source_id);
    });
}

/// Queues a switch to `target` after the settle window, replacing any pending
/// switch to a different target.
fn request_source_switch(ctx: &mut ViewContext<Workspace>, target: &str, trigger: &'static str) {
    let seq = InputSourceMemoryModel::handle(ctx).update(ctx, |model, _| {
        if model.pending.as_ref().is_some_and(|p| p.target == target) {
            return None;
        }
        model.switch_seq += 1;
        model.pending = Some(PendingSwitch {
            target: target.to_owned(),
            trigger,
            seq: model.switch_seq,
        });
        Some(model.switch_seq)
    });
    let Some(seq) = seq else { return };
    ctx.spawn(Timer::after(SWITCH_SETTLE), move |_, _deadline, ctx| {
        let fired = InputSourceMemoryModel::handle(ctx).update(ctx, |model, _| {
            match model.pending.take() {
                Some(p) if p.seq == seq => Some((p.target, p.trigger)),
                other => {
                    model.pending = other;
                    None
                }
            }
        });
        if let Some((target, trigger)) = fired {
            perform_switch(ctx, &target, trigger);
        }
    });
}

/// Executes a deferred switch unless the current source already matches, and
/// logs the request with its trigger so logs show which focus event caused a
/// (possibly slow) platform-side selection.
fn perform_switch(ctx: &mut AppContext, target: &str, trigger: &'static str) {
    let Some(current) = current_input_source_id(ctx) else {
        return;
    };
    if current == target {
        return;
    }
    log::info!("input-source switch requested ({trigger}): {current} -> {target}");
    ctx.select_input_source(target);
}

/// Whether any title source for the pane matches the exemption rule. The
/// displayed pane title can be overridden (CLI-agent event text, warpified
/// SSH conversations), so the custom rename and the raw OSC title are checked
/// as well. Empty sources never count as a match, keeping negative patterns
/// from exempting everything.
fn exemption_matches(
    view: &TerminalView,
    pane_config: &PaneConfiguration,
    mode: PaneTitleMatchMode,
    pattern: &str,
) -> bool {
    let sources = [
        Some(pane_config.title()),
        pane_config.custom_vertical_tabs_title(),
    ];
    if sources
        .into_iter()
        .flatten()
        .any(|title| !title.is_empty() && title_is_exempt(title, mode, pattern))
    {
        return true;
    }
    let raw_osc = view.terminal_title_from_shell();
    !raw_osc.is_empty() && title_is_exempt(&raw_osc, mode, pattern)
}

/// Applies the rule for `pane_id`'s title: restore its remembered source when
/// exempt, otherwise force English.
fn apply_for_pane(
    pane_group: &ViewHandle<PaneGroup>,
    pane_id: PaneId,
    trigger: &'static str,
    ctx: &mut ViewContext<Workspace>,
) {
    let pane_group = pane_group.as_ref(ctx);
    let Some(terminal_view) = pane_group.terminal_view_from_pane_id(pane_id, ctx) else {
        return;
    };
    let view = terminal_view.as_ref(ctx);
    let pane_config = view.pane_configuration().as_ref(ctx);

    let settings = GeneralSettings::as_ref(ctx);
    let mode = settings.pane_title_match_mode;
    let pattern = settings.pane_title_match_pattern.value().clone();

    if exemption_matches(view, pane_config, mode, &pattern) {
        let remembered = InputSourceMemoryModel::handle(ctx)
            .as_ref(ctx)
            .remembered
            .get(&pane_id)
            .cloned();
        if let Some(source_id) = remembered {
            request_source_switch(ctx, &source_id, trigger);
        }
    } else {
        request_source_switch(ctx, ENGLISH_INPUT_SOURCE_ID, trigger);
    }
}

/// Called when a pane gains focus within a window. Snapshots the outgoing
/// pane's current input source (still reflects the old pane's state because we
/// run before any forced switch), then applies the rule for the new pane.
pub fn on_pane_focused(
    pane_group: &ViewHandle<PaneGroup>,
    window_id: WindowId,
    ctx: &mut ViewContext<Workspace>,
) {
    if !policy_enabled(ctx) {
        return;
    }

    let focused = pane_group.as_ref(ctx).focused_pane_id(ctx);
    let previous = InputSourceMemoryModel::handle(ctx)
        .as_ref(ctx)
        .last_focused
        .get(&window_id)
        .copied();
    if let Some(previous) = previous.filter(|p| *p != focused)
        && let Some(source_id) = current_input_source_id(ctx)
    {
        remember_source(ctx, previous, &source_id);
    }
    InputSourceMemoryModel::handle(ctx).update(ctx, |model, _| {
        model.last_focused.insert(window_id, focused);
    });

    apply_for_pane(pane_group, focused, "pane-focused", ctx);
}

/// Called when this window becomes the active window. No outgoing-pane
/// snapshot: while Warp was inactive the system input source may have changed
/// for another app's sake.
pub fn on_window_activated(pane_group: &ViewHandle<PaneGroup>, ctx: &mut ViewContext<Workspace>) {
    if !policy_enabled(ctx) {
        return;
    }
    let focused = pane_group.as_ref(ctx).focused_pane_id(ctx);
    apply_for_pane(pane_group, focused, "window-activated", ctx);
}

/// Called when this window is losing active-window status: snapshot the
/// focused pane's input source while it still reflects that pane's state.
pub fn on_window_deactivated(pane_group: &ViewHandle<PaneGroup>, ctx: &mut AppContext) {
    if !policy_enabled(ctx) {
        return;
    }
    let focused = pane_group.as_ref(ctx).focused_pane_id(ctx);
    if let Some(source_id) = current_input_source_id(ctx) {
        remember_source(ctx, focused, &source_id);
    }
}

#[cfg(test)]
#[path = "input_source_policy_tests.rs"]
mod tests;
