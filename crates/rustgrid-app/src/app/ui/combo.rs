//! A searchable drop-down (combo box), backed by gpui-kit's shadcn [`Combobox`].
//!
//! The host owns one [`ComboBox`] entity per field and drives it through
//! [`ComboBox::set_options`] / [`ComboBox::set_selected`], reacting to a pick through
//! [`ComboBox::on_select`]. The popup, search field, keyboard navigation and overlay all come
//! from the component library.
//!
//! gpui-kit's `ComboboxState` needs a `Window` to be constructed, but the app builds its combos
//! while the view is constructed. The state is therefore created on the first render and pending
//! option/selection changes are flushed there.

use std::rc::Rc;

use gpui::{
    App, Context, Entity, IntoElement, Render, SharedString, Subscription, Window, div, prelude::*,
    px, rgb, svg,
};
use gpui_kit::component::Icon;
use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::{FocusableExt, Sizable};

use crate::theme::Theme;

/// Called with the chosen option's value after the user picks it.
pub(crate) type ComboSelectCallback = Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>;

/// One selectable row: `value` is the machine value, `label` is what the user sees.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ComboOption {
    pub value: String,
    pub label: String,
    /// An optional leading glyph (an engine mark for a connection, a database for a schema, ...).
    icon: Option<(&'static str, u32)>,
}

impl ComboOption {
    pub(crate) fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            icon: None,
        }
    }

    /// A row whose value and label are the same string.
    pub(crate) fn plain(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            icon: None,
        }
    }

    /// Draw `icon` (tinted with `color`) before the label in both the dropdown row and — via
    /// [`ComboBox::set_icon`] — the trigger when the caller wants that.
    pub(crate) fn with_icon(mut self, icon: &'static str, color: u32) -> Self {
        self.icon = Some((icon, color));
        self
    }
}

impl SearchableListItem for ComboOption {
    type Value = String;

    fn title(&self) -> SharedString {
        SharedString::from(self.label.clone())
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Some((icon, color)) = self.icon else {
            return self.title().into_any_element();
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                svg()
                    .path(icon)
                    .w(px(14.0))
                    .h(px(14.0))
                    .flex_none()
                    .text_color(rgb(color)),
            )
            .child(SharedString::from(self.label.clone()))
            .into_any_element()
    }
}

type ComboDelegate = SearchableVec<ComboOption>;
type ComboState = ComboboxState<ComboDelegate>;

pub(crate) struct ComboBox {
    theme: Theme,
    width: f32,
    /// When set, the trigger field is pinned to this width instead of sizing to its content
    /// (the dropdown menu always uses `width`).
    field_width: Option<f32>,
    /// Stretches the trigger field to fill its parent instead of sizing to its content.
    full_width: bool,
    /// Optional trigger height / text size / corner radius overrides (the filter builder renders
    /// its selects taller than the app's compact default).
    height: Option<f32>,
    text_size: Option<f32>,
    radius: Option<f32>,
    enabled: bool,
    placeholder: SharedString,
    icon: Option<(&'static str, u32)>,
    selected: String,
    options: Vec<ComboOption>,
    on_select: Option<ComboSelectCallback>,
    /// The gpui-kit state, created on the first render.
    state: Option<Entity<ComboState>>,
    subscriptions: Vec<Subscription>,
    pending_options: bool,
    pending_selected: bool,
    /// A pick reported by the kit, waiting for the next render to reach the callback.
    pending_select: Option<String>,
}

impl ComboBox {
    pub(crate) fn new(
        theme: Theme,
        options: Vec<ComboOption>,
        selected: impl Into<String>,
        width: f32,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            theme,
            width,
            field_width: None,
            full_width: false,
            height: None,
            text_size: None,
            radius: None,
            enabled: true,
            placeholder: SharedString::default(),
            icon: None,
            selected: selected.into(),
            options,
            on_select: None,
            state: None,
            subscriptions: Vec::new(),
            pending_options: false,
            pending_selected: false,
            pending_select: None,
        }
    }

    pub(crate) fn on_select(mut self, callback: ComboSelectCallback) -> Self {
        self.on_select = Some(callback);
        self
    }

    /// Pins the trigger field to a fixed width, so an empty selection does not collapse it.
    pub(crate) fn field_width(mut self, width: f32) -> Self {
        self.field_width = Some(width);
        self
    }

    /// Stretches the trigger field to fill its parent (used by dialogs, so a combo matches the
    /// width of the text fields above it).
    pub(crate) fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    /// Overrides the trigger height (the filter builder renders its selects at 40px).
    pub(crate) fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }

    /// Overrides the trigger's font size, independent of the height.
    pub(crate) fn text_size(mut self, size: f32) -> Self {
        self.text_size = Some(size);
        self
    }

    /// Overrides the trigger's corner radius.
    pub(crate) fn radius(mut self, radius: f32) -> Self {
        self.radius = Some(radius);
        self
    }

    /// Replaces the list of rows and re-applies the current selection.
    pub(crate) fn set_options(&mut self, options: Vec<ComboOption>, cx: &mut Context<Self>) {
        if self.options == options {
            return;
        }
        self.options = options;
        self.pending_options = true;
        self.pending_selected = true;
        cx.notify();
    }

    /// Sets the committed value.
    pub(crate) fn set_selected(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        let value = value.into();
        if self.selected == value {
            return;
        }
        self.selected = value;
        self.pending_selected = true;
        cx.notify();
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        cx.notify();
    }

    pub(crate) fn set_icon(&mut self, icon: &'static str, color: u32, cx: &mut Context<Self>) {
        if self.icon == Some((icon, color)) {
            return;
        }
        self.icon = Some((icon, color));
        cx.notify();
    }

    pub(crate) fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        if self.placeholder == placeholder {
            return;
        }
        self.placeholder = placeholder;
        cx.notify();
    }

    pub(crate) fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        if self.theme == theme {
            return;
        }
        self.theme = theme;
        cx.notify();
    }

    /// Creates the inner gpui-kit state on the first render and wires its events.
    fn ensure_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.is_some() {
            return;
        }
        let items = SearchableVec::new(self.options.clone());
        let state = cx.new(|cx| {
            ComboboxState::new(items, vec![], window, cx)
                .multiple(false)
                .searchable(true)
        });
        self.subscriptions.push(cx.subscribe(
            &state,
            |this, _state, event: &ComboboxEvent<ComboDelegate>, cx| {
                if let ComboboxEvent::Change(values) = event
                    && let Some(value) = values.first()
                {
                    this.pending_select = Some(value.clone());
                    cx.notify();
                }
            },
        ));
        self.state = Some(state);
        self.pending_options = true;
        self.pending_selected = true;
    }

    /// Applies queued mutations and dispatches queued callbacks, which need a `Window`.
    fn flush(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };

        if self.pending_options {
            self.pending_options = false;
            let items = SearchableVec::new(self.options.clone());
            state.update(cx, |state, cx| state.set_items(items, window, cx));
        }
        if self.pending_selected {
            self.pending_selected = false;
            let selected = self.selected.clone();
            state.update(cx, |state, cx| {
                state.set_selected_values(std::slice::from_ref(&selected), window, cx);
            });
        }

        if let Some(value) = self.pending_select.take()
            && let Some(callback) = self.on_select.clone()
        {
            callback(&value, window, cx);
        }
    }
}

impl Render for ComboBox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_state(window, cx);
        self.flush(window, cx);
        let state = self.state.clone().expect("combo state");

        let mut combo = Combobox::new(&state)
            .placeholder(self.placeholder.clone())
            .menu_width(px(self.width))
            .cleanable(false)
            .disabled(!self.enabled)
            .focus_ring(false)
            .xsmall();
        if let Some(height) = self.height {
            combo = combo.h(px(height));
        }
        if let Some(size) = self.text_size {
            combo = combo.text_size(px(size));
        }
        if let Some(radius) = self.radius {
            combo = combo.rounded(px(radius));
        }
        if let Some((icon, color)) = self.icon {
            combo = combo.icon(Icon::default().path(icon).text_color(rgb(color)));
        }

        let mut container = div();
        if self.full_width {
            container = container.w_full();
        } else if let Some(width) = self.field_width {
            container = container.w(px(width));
        }
        container.child(combo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Point, TestAppContext, WindowBounds, WindowOptions, size};
    use gpui_kit::TestSupportExt as _;
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt as _;

    struct ComboHarness {
        combo: Entity<ComboBox>,
        show: bool,
    }

    impl Render for ComboHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut root = div().size_full().p_4();
            if self.show {
                root = root.child(
                    div()
                        .id("combo-host")
                        .test_support()
                        .w(px(300.0))
                        .h(px(24.0))
                        .child(self.combo.clone()),
                );
            }
            root
        }
    }

    fn options() -> Vec<ComboOption> {
        vec![
            ComboOption::plain("macce"),
            ComboOption::plain("sjis"),
            ComboOption::plain("utf8mb4"),
        ]
    }

    fn query(harness: &Entity<ComboHarness>, cx: &App) -> String {
        let combo = harness.read(cx).combo.read(cx);
        let state = combo.state.clone().expect("combo state");
        state.read(cx).query(cx).to_string()
    }

    fn open(cx: &mut TestAppContext) -> (gpui::AnyWindowHandle, Entity<ComboHarness>) {
        cx.update(gpui_kit::init);
        cx.update(|cx| {
            let combo = cx.new(|cx| {
                ComboBox::new(Theme::light(), options(), String::new(), 300.0, cx)
                    .field_width(300.0)
            });
            let harness = cx.new(|_| ComboHarness { combo, show: true });
            let (handle, _) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(800.0), px(600.0)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, _| harness.clone(),
            )
            .expect("open test window");
            (handle, harness)
        })
    }

    #[gpui_kit::test]
    fn combo_search_receives_input_in_a_plain_window(cx: &mut TestAppContext) {
        let (handle, harness) = open(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("combo-host", cx);
            window.render_frame(cx);
            window.input("mac", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(query(&harness, cx), "mac"));
    }

    #[gpui_kit::test]
    fn combo_search_receives_input_inside_a_dialog(cx: &mut TestAppContext) {
        let (handle, harness) = open(cx);
        let combo = cx.read(|cx| harness.read(cx).combo.clone());
        // The harness must not render the combo itself, or it would appear twice (behind the
        // dialog and inside it).
        harness.update(cx, |harness, cx| {
            harness.show = false;
            cx.notify();
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.open_dialog(cx, move |dialog, _window, _cx| {
                let combo = combo.clone();
                dialog.title("Test").content(move |content, _window, _cx| {
                    content.child(
                        div()
                            .id("combo-host")
                            .test_support()
                            .w(px(300.0))
                            .h(px(24.0))
                            .child(combo.clone()),
                    )
                })
            });
            window.render_frame(cx);
        })
        .unwrap();

        cx.update_window(handle, |_, window, cx| {
            window.click("combo-host", cx);
            window.render_frame(cx);
            window.input("mac", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(query(&harness, cx), "mac"));
    }

    /// The New/Edit Database dialog keeps a name field focused once and then lets the user move to
    /// the combo box. Its builder runs every frame, so the one-shot focus must survive across
    /// frames — otherwise the name field is re-focused each frame and the combo's search box can
    /// never hold the focus (this is what `AppView::dialog_focus_done` guards).
    #[gpui_kit::test]
    fn combo_search_survives_a_dialog_that_focuses_its_field_once(cx: &mut TestAppContext) {
        use crate::app::dialogs::take_dialog_focus;
        use crate::app::ui::{TextInput, TextInputOptions};
        use std::cell::RefCell;

        let (handle, harness) = open(cx);
        let combo = cx.read(|cx| harness.read(cx).combo.clone());
        harness.update(cx, |harness, cx| {
            harness.show = false;
            cx.notify();
        });

        let name = cx.new(|cx| {
            TextInput::new(
                Theme::light(),
                String::new(),
                TextInputOptions::default(),
                cx,
            )
        });
        let focused = Rc::new(RefCell::new(false));
        let name_dialog = name.clone();
        let combo_dialog = combo.clone();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.open_dialog(cx, move |dialog, _window, _cx| {
                let name = name_dialog.clone();
                let combo = combo_dialog.clone();
                let focused = focused.clone();
                dialog.title("Test").content(move |content, window, cx| {
                    if take_dialog_focus(&mut focused.borrow_mut()) {
                        name.update(cx, |input, cx| input.focus_state(window, cx));
                    }
                    content.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .id("name-host")
                                    .test_support()
                                    .w(px(300.0))
                                    .h(px(24.0))
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .id("combo-host")
                                    .test_support()
                                    .w(px(300.0))
                                    .h(px(24.0))
                                    .child(combo.clone()),
                            ),
                    )
                })
            });
            window.render_frame(cx);
        })
        .unwrap();

        cx.update_window(handle, |_, window, cx| {
            window.click("combo-host", cx);
            window.render_frame(cx);
            window.input("mac", cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(query(&harness, cx), "mac"));
    }
}
