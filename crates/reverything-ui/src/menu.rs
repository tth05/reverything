//! A compact context menu like Explorer's: 22px rows, small text, icons in a column on the left
//! and shortcuts on the right. The component library's menu has no size between its default and
//! roomy rows, so the result rows and Explorer's entries use this one.

use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme, Icon, IconName, Sizable, StyledExt, ThemeStyled,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Height of an entry
const ROW_HEIGHT: Pixels = px(22.);
/// The icon column
const ICON_SIZE: Pixels = px(16.);
/// Space between the menu's border and its entries
const PADDING: Pixels = px(3.);

pub enum MenuIcon {
    None,
    Svg(IconName),
    Image(Arc<RenderImage>),
}

/// What an entry does when it is chosen. Runs after the menu closed.
pub type Run = Rc<dyn Fn(&mut Window, &mut App)>;

pub enum Entry {
    Separator,
    Item {
        icon: MenuIcon,
        label: SharedString,
        /// Shown on the right, e.g. `Ctrl+C`
        shortcut: Option<SharedString>,
        disabled: bool,
        checked: bool,
        /// Shown in bold, like the entry Explorer runs on a double click
        bold: bool,
        run: Run,
    },
    /// Its entries are built when it opens
    Submenu {
        icon: MenuIcon,
        label: SharedString,
        entries: Rc<dyn Fn() -> Vec<Entry>>,
    },
}

impl Entry {
    pub fn item(
        icon: MenuIcon,
        label: impl Into<SharedString>,
        run: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Entry::Item {
            icon,
            label: label.into(),
            shortcut: None,
            disabled: false,
            checked: false,
            bold: false,
            run: Rc::new(run),
        }
    }

    /// Sets the shortcut shown on the right of an item.
    pub fn shortcut(mut self, text: &'static str) -> Self {
        if let Entry::Item { shortcut, .. } = &mut self {
            *shortcut = Some(text.into());
        }
        self
    }

    fn selectable(&self) -> bool {
        match self {
            Entry::Separator => false,
            Entry::Item { disabled, .. } => !disabled,
            Entry::Submenu { .. } => true,
        }
    }

    fn has_icon(&self) -> bool {
        match self {
            Entry::Separator => false,
            Entry::Item { icon, checked, .. } => *checked || !matches!(icon, MenuIcon::None),
            Entry::Submenu { icon, .. } => !matches!(icon, MenuIcon::None),
        }
    }
}

pub struct Menu {
    entries: Vec<Entry>,
    focus: FocusHandle,
    /// Entry under the mouse or chosen with the arrow keys
    selected: Option<usize>,
    /// The open submenu and the entry it belongs to
    submenu: Option<(usize, Entity<Menu>)>,
    /// The top menu, which closes the whole chain
    root: Option<WeakEntity<Menu>>,
    /// Where this menu was drawn, so a click into a submenu does not close the menu
    bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    depth: usize,
}

impl EventEmitter<DismissEvent> for Menu {}

impl Focusable for Menu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Menu {
    pub fn new(entries: Vec<Entry>, cx: &mut Context<Self>) -> Self {
        Self {
            entries,
            focus: cx.focus_handle(),
            selected: None,
            submenu: None,
            root: None,
            bounds: Rc::default(),
            depth: 0,
        }
    }

    /// Whether `position` is on this menu or one of its open submenus.
    fn contains(&self, position: Point<Pixels>, cx: &App) -> bool {
        self.bounds.get().contains(&position)
            || self
                .submenu
                .as_ref()
                .is_some_and(|(_, menu)| menu.read(cx).contains(position, cx))
    }

    /// Closes the whole chain of menus.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        match self.root.as_ref().and_then(|root| root.upgrade()) {
            Some(root) => root.update(cx, |_, cx| cx.emit(DismissEvent)),
            None => cx.emit(DismissEvent),
        }
    }

    fn choose(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.entries.get(ix) {
            Some(Entry::Item {
                run,
                disabled: false,
                ..
            }) => {
                let run = run.clone();
                self.dismiss(cx);
                // After the menu is gone and the focus is back where it was
                window.defer(cx, move |window, cx| run(window, cx));
            }
            Some(Entry::Submenu { .. }) => self.open_submenu(ix, true, window, cx),
            _ => {}
        }
    }

    fn open_submenu(
        &mut self,
        ix: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.submenu.as_ref().is_some_and(|(open, _)| *open == ix) {
            if focus {
                if let Some((_, menu)) = &self.submenu {
                    menu.update(cx, |menu, cx| {
                        menu.selected = menu.entries.iter().position(Entry::selectable);
                        menu.focus.focus(window, cx);
                    });
                }
            }
            return;
        }
        let Some(Entry::Submenu { entries, .. }) = self.entries.get(ix) else {
            self.submenu = None;
            return;
        };
        let entries = entries();
        let root = self.root.clone().unwrap_or_else(|| cx.entity().downgrade());
        let depth = self.depth + 1;
        let menu = cx.new(|cx| {
            let mut menu = Menu::new(entries, cx);
            menu.root = Some(root);
            menu.depth = depth;
            menu
        });
        if focus {
            menu.update(cx, |menu, cx| {
                menu.selected = menu.entries.iter().position(Entry::selectable);
                menu.focus.focus(window, cx);
            });
        }
        self.submenu = Some((ix, menu));
        cx.notify();
    }

    /// The next selectable entry from the selected one in `step` direction, wrapping around.
    fn step(&mut self, step: isize) {
        let n = self.entries.len() as isize;
        if n == 0 {
            return;
        }
        let mut ix = self
            .selected
            .map_or(if step > 0 { -1 } else { n }, |s| s as isize);
        for _ in 0..n {
            ix = (ix + step).rem_euclid(n);
            if self.entries[ix as usize].selectable() {
                self.selected = Some(ix as usize);
                return;
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Only the menu with the focus, the parents get the keys of their submenus too
        if !self.focus.is_focused(window) {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.step(1),
            "up" => self.step(-1),
            "enter" | "space" => {
                if let Some(ix) = self.selected {
                    self.choose(ix, window, cx);
                }
            }
            "right" => {
                if let Some(ix) = self.selected {
                    if matches!(self.entries.get(ix), Some(Entry::Submenu { .. })) {
                        self.open_submenu(ix, true, window, cx);
                    }
                }
            }
            "left" if self.depth > 0 => {
                // The parent closes this submenu and takes the focus back
                if let Some(parent) = self.root.as_ref().and_then(|r| r.upgrade()) {
                    close_deepest(&parent, window, cx);
                }
                return;
            }
            "escape" => self.dismiss(cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn render_entry(
        &self,
        ix: usize,
        entry: &Entry,
        icons: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let icon = |icon: &MenuIcon, checked: bool| -> AnyElement {
            match icon {
                MenuIcon::Svg(name) => Icon::new(name.clone())
                    .small()
                    .text_color(theme.muted_foreground)
                    .into_any_element(),
                MenuIcon::Image(image) => img(image.clone()).size(ICON_SIZE).into_any_element(),
                MenuIcon::None if checked => Icon::new(IconName::Check).small().into_any_element(),
                MenuIcon::None => div().size(ICON_SIZE).into_any_element(),
            }
        };
        let (label, entry_icon, shortcut, disabled, checked, bold, submenu) = match entry {
            Entry::Separator => {
                return div()
                    .h(px(1.))
                    .my(px(3.))
                    .mx(px(4.))
                    .bg(theme.border)
                    .into_any_element()
            }
            Entry::Item {
                icon,
                label,
                shortcut,
                disabled,
                checked,
                bold,
                ..
            } => (
                label,
                icon,
                shortcut.clone(),
                *disabled,
                *checked,
                *bold,
                false,
            ),
            Entry::Submenu { icon, label, .. } => (label, icon, None, false, false, false, true),
        };
        let selected = self.selected == Some(ix) && !disabled;
        let open = self.submenu.as_ref().filter(|(open, _)| *open == ix);
        h_flex()
            .id(("entry", ix))
            .relative()
            .h(ROW_HEIGHT)
            .px(px(6.))
            .gap(px(8.))
            .rounded(theme.radius / 2.)
            .text_xs()
            .when(disabled, |row| row.text_color(theme.muted_foreground))
            .when(selected || open.is_some(), |row| {
                row.bg(theme.accent).text_color(theme.accent_foreground)
            })
            .when(icons, |row| row.child(icon(entry_icon, checked)))
            .child(
                div()
                    .flex_1()
                    .whitespace_nowrap()
                    .when(bold, |label| label.font_semibold())
                    .child(label.clone()),
            )
            .children(shortcut.map(|shortcut| {
                div()
                    .ml(px(24.))
                    .text_color(theme.muted_foreground)
                    .child(shortcut)
            }))
            .when(submenu, |row| {
                row.child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
            })
            .on_hover(cx.listener(move |menu, hovered: &bool, window, cx| {
                if !*hovered {
                    return;
                }
                menu.selected = Some(ix);
                if submenu {
                    menu.open_submenu(ix, false, window, cx);
                } else {
                    menu.submenu = None;
                }
                cx.notify();
            }))
            .when(!disabled, |row| {
                row.on_click(cx.listener(move |menu, _, window, cx| menu.choose(ix, window, cx)))
            })
            // The submenu opens next to its entry
            .children(open.map(|(_, menu)| {
                div().absolute().top(-PADDING).left(relative(1.)).child(
                    deferred(
                        anchored()
                            .snap_to_window_with_margin(px(4.))
                            .child(div().occlude().child(menu.clone())),
                    )
                    .with_priority(self.depth + 2),
                )
            }))
            .into_any_element()
    }
}

/// Closes the deepest open submenu below `menu` and focuses its parent.
fn close_deepest(menu: &Entity<Menu>, window: &mut Window, cx: &mut App) {
    let child = menu.read(cx).submenu.as_ref().map(|(_, m)| m.clone());
    match child {
        Some(child) if child.read(cx).submenu.is_some() => close_deepest(&child, window, cx),
        Some(_) => menu.update(cx, |menu, cx| {
            menu.submenu = None;
            menu.focus.focus(window, cx);
            cx.notify();
        }),
        None => {}
    }
}

impl Render for Menu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let icons = self.entries.iter().any(Entry::has_icon);
        let entries = (0..self.entries.len())
            .map(|ix| {
                let entry = &self.entries[ix];
                self.render_entry(ix, entry, icons, cx)
            })
            .collect::<Vec<_>>();
        let bounds = self.bounds.clone();
        v_flex()
            .id("menu")
            .key_context("Menu")
            .track_focus(&self.focus)
            .occlude()
            .popover_style(cx)
            .p(PADDING)
            .min_w(px(180.))
            .on_key_down(cx.listener(Self::on_key))
            .when(self.depth == 0, |menu| {
                menu.on_mouse_down_out(cx.listener(|menu, event: &MouseDownEvent, _, cx| {
                    if !menu.contains(event.position, cx) {
                        menu.dismiss(cx);
                    }
                }))
            })
            .on_hover(cx.listener(|menu, hovered: &bool, _, cx| {
                // Leaving the menu keeps an open submenu's entry selected
                if !*hovered && menu.submenu.is_none() {
                    menu.selected = None;
                    cx.notify();
                }
            }))
            .children(entries)
            .child(
                canvas(move |area, _, _| bounds.set(area), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
    }
}
