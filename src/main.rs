mod gpu;
mod groups;
mod nvml;
mod performance;

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use gtk::{gio, glib, prelude::*};
use serde::{Deserialize, Serialize};
use url::Url;
use webkit6::{WebView, prelude::*};

const APP_ID: &str = "io.github.cdomin1.Tillwind";
const BLANK: &str = "about:blank";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Appearance {
    Light,
    Dark,
    #[default]
    #[serde(other)]
    System,
}

impl Appearance {
    fn is_system(&self) -> bool {
        matches!(self, Self::System)
    }

    fn value(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn apply(self, window: &gtk::ApplicationWindow) {
        window.remove_css_class("appearance-light");
        window.remove_css_class("appearance-dark");
        if let Some(settings) = gtk::Settings::default() {
            if self.is_system() {
                settings.reset_property("gtk-application-prefer-dark-theme");
            } else {
                settings.set_gtk_application_prefer_dark_theme(matches!(self, Self::Dark));
                window.add_css_class(if matches!(self, Self::Dark) {
                    "appearance-dark"
                } else {
                    "appearance-light"
                });
            }
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Session {
    urls: Vec<String>,
    selected: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    groups: Vec<groups::Group>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tab_groups: Vec<Option<u64>>,
    #[serde(default, skip_serializing_if = "Appearance::is_system")]
    appearance: Appearance,
}

impl Session {
    fn path() -> PathBuf {
        glib::user_data_dir().join("tillwind/session.json")
    }

    fn read() -> Self {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::path();
        std::fs::create_dir_all(glib::user_data_dir().join("tillwind"))?;
        gio::File::for_path(path).replace_contents(
            &serde_json::to_vec(self)?,
            None,
            false,
            gio::FileCreateFlags::PRIVATE,
            gio::Cancellable::NONE,
        )?;
        Ok(())
    }
}

struct Tab {
    page: gtk::Overlay,
    content: gtk::Box,
    label: gtk::Label,
    favicon: gtk::Image,
    uri: RefCell<String>,
    view: RefCell<Option<WebView>>,
    group: Cell<Option<u64>>,
}

struct Browser {
    notebook: gtk::Notebook,
    address: gtk::Entry,
    address_focus: gtk::EventControllerFocus,
    back: gtk::Button,
    forward: gtk::Button,
    tabs: RefCell<Vec<Rc<Tab>>>,
    performance: Rc<performance::Performance>,
    performance_timer: RefCell<Option<glib::SourceId>>,
    groups: groups::TabGroups,
    group_label: gtk::Label,
    appearance: Cell<Appearance>,
}

fn web_uri(input: &str) -> Option<Url> {
    Url::parse(input)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
}

fn address_target(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input == BLANK {
        return Some(BLANK.into());
    }
    if let Some(url) = web_uri(input) {
        return Some(url.into());
    }
    if !input.contains(char::is_whitespace) && !input.contains("://") {
        if let Some(url) = web_uri(&format!("https://{input}")) {
            if url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| host.contains('.') || host == "localhost")
            {
                return Some(url.into());
            }
        }
    }
    let mut search = Url::parse("https://duckduckgo.com/").expect("valid search endpoint");
    search.query_pairs_mut().append_pair("q", input);
    Some(search.into())
}

fn icon(name: &str, description: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(name);
    button.set_tooltip_text(Some(description));
    button.update_property(&[gtk::accessible::Property::Label(description)]);
    button.add_css_class("flat");
    button
}

impl Browser {
    fn tab_for(&self, page: &gtk::Widget) -> Option<Rc<Tab>> {
        self.tabs
            .borrow()
            .iter()
            .find(|tab| tab.page.upcast_ref::<gtk::Widget>() == page)
            .cloned()
    }

    fn current(&self) -> Option<Rc<Tab>> {
        self.notebook
            .nth_page(self.notebook.current_page())
            .and_then(|page| self.tab_for(&page))
    }

    fn sync_address(&self, tab: &Tab, editing: bool) {
        let uri = tab.uri.borrow();
        gtk::prelude::EntryExt::set_alignment(&self.address, if editing { 0.0 } else { 0.5 });
        if uri.as_str() == BLANK {
            self.address.set_text("");
        } else if !editing && let Some(url) = web_uri(&uri) {
            self.address.set_text(url.host_str().unwrap_or(&uri));
        } else {
            self.address.set_text(&uri);
        }
        self.address
            .set_tooltip_text((uri.as_str() != BLANK).then_some(uri.as_str()));
    }

    fn sync_navigation(&self, tab: &Tab) {
        let view = tab.view.borrow();
        self.back
            .set_sensitive(view.as_ref().is_some_and(|view| view.can_go_back()));
        self.forward
            .set_sensitive(view.as_ref().is_some_and(|view| view.can_go_forward()));
    }

    fn add_tab(self: &Rc<Self>, uri: String, group: Option<u64>) -> u32 {
        let title = web_uri(&uri)
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "New Tab".into());
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let page = gtk::Overlay::new();
        page.set_child(Some(&content));
        let tab = Rc::new(Tab {
            page,
            content,
            label: gtk::Label::new(Some(&title)),
            favicon: gtk::Image::from_icon_name("web-browser-symbolic"),
            uri: RefCell::new(uri),
            view: RefCell::new(None),
            group: Cell::new(group),
        });
        tab.label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        tab.label.set_max_width_chars(24);
        tab.label.set_hexpand(true);
        tab.label.set_tooltip_text(Some(&title));
        tab.favicon.set_pixel_size(14);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let close = icon("window-close-symbolic", "Close tab");
        close.add_css_class("tab-close");
        let title_group = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title_group.set_halign(gtk::Align::Center);
        title_group.set_hexpand(true);
        title_group.append(&tab.favicon);
        title_group.append(&tab.label);
        header.append(&close);
        header.append(&title_group);
        close.connect_clicked(glib::clone!(
            #[weak(rename_to = browser)]
            self,
            #[weak]
            tab,
            move |_| browser.close_tab(&tab)
        ));
        self.tabs.borrow_mut().push(tab.clone());
        let index = self.notebook.append_page(&tab.page, Some(&header));
        self.notebook.page(&tab.page).set_tab_expand(true);
        self.notebook.set_tab_reorderable(&tab.page, true);
        index
    }

    fn close_tab(self: &Rc<Self>, tab: &Tab) {
        if self.tabs_in_group(tab.group.get()).len() == 1 {
            self.new_tab();
        }
        if let Some(index) = self.notebook.page_num(&tab.page) {
            self.notebook.remove_page(Some(index));
        }
        self.tabs
            .borrow_mut()
            .retain(|other| other.page != tab.page);
    }

    fn new_tab(self: &Rc<Self>) {
        let index = self.add_tab(BLANK.into(), self.groups.active.get());
        self.notebook.set_current_page(Some(index));
        self.address.grab_focus();
    }

    fn activate_tab(self: &Rc<Self>, tab: &Rc<Tab>) {
        self.performance.attach(&tab.page);
        if tab.view.borrow().is_none() {
            let policies = webkit6::WebsitePolicies::builder()
                .autoplay(webkit6::AutoplayPolicy::Deny)
                .build();
            let view = WebView::builder()
                .website_policies(&policies)
                .vexpand(true)
                .build();
            let recovery = gtk::Box::new(gtk::Orientation::Vertical, 12);
            recovery.set_valign(gtk::Align::Center);
            recovery.set_vexpand(true);
            recovery.append(&gtk::Label::new(Some("This page stopped running.")));
            let reload = gtk::Button::with_label("Reload Page");
            reload.set_halign(gtk::Align::Center);
            recovery.append(&reload);
            recovery.set_visible(false);
            tab.content.append(&view);
            tab.content.append(&recovery);
            reload.connect_clicked(glib::clone!(
                #[weak]
                view,
                #[weak]
                recovery,
                #[weak]
                tab,
                move |_| {
                    recovery.set_visible(false);
                    view.set_visible(true);
                    let uri = tab.uri.borrow().clone();
                    view.load_uri(&uri);
                }
            ));
            view.connect_web_process_terminated(glib::clone!(
                #[weak]
                recovery,
                move |view, _| {
                    view.set_visible(false);
                    recovery.set_visible(true);
                }
            ));
            view.connect_load_changed(glib::clone!(
                #[weak(rename_to = browser)]
                self,
                #[weak]
                tab,
                #[weak]
                recovery,
                move |view, event| {
                    if event == webkit6::LoadEvent::Started {
                        recovery.set_visible(false);
                        view.set_visible(true);
                    }
                    if let Some(uri) = view.uri() {
                        if web_uri(&uri).is_some() || uri == BLANK {
                            *tab.uri.borrow_mut() = uri.to_string();
                        }
                    }
                    if browser
                        .current()
                        .is_some_and(|current| Rc::ptr_eq(&current, &tab))
                    {
                        if !browser.address_focus.contains_focus() {
                            browser.sync_address(&tab, false);
                        }
                        browser.sync_navigation(&tab);
                    }
                }
            ));
            view.connect_title_notify(glib::clone!(
                #[weak]
                tab,
                move |view| {
                    if let Some(title) = view.title() {
                        tab.label.set_label(&title);
                        tab.label.set_tooltip_text(Some(&title));
                    }
                }
            ));
            view.connect_favicon_notify(glib::clone!(
                #[weak]
                tab,
                move |view| {
                    if let Some(favicon) = view.favicon() {
                        tab.favicon.set_paintable(Some(&favicon));
                    } else {
                        tab.favicon.set_icon_name(Some("web-browser-symbolic"));
                    }
                }
            ));
            *tab.view.borrow_mut() = Some(view.clone());
            let uri = tab.uri.borrow().clone();
            view.load_uri(&uri);
        }
        self.sync_address(tab, self.address_focus.contains_focus());
        self.sync_navigation(tab);
    }

    fn refresh_performance(&self) {
        if self.performance.widget.is_visible()
            && let Some(tab) = self.current()
            && let Some(view) = tab.view.borrow().as_ref()
        {
            let uri = tab.uri.borrow();
            let site = web_uri(&uri);
            let host = site.as_ref().and_then(Url::host_str).unwrap_or("New Tab");
            self.performance.refresh(view, host);
        }
    }

    fn save(&self) {
        let ordered: Vec<_> = (0..self.notebook.n_pages())
            .filter_map(|index| self.notebook.nth_page(Some(index)))
            .filter_map(|page| self.tab_for(&page))
            .collect();
        let mut tab_groups: Vec<_> = ordered.iter().map(|tab| tab.group.get()).collect();
        if tab_groups.iter().all(Option::is_none) {
            tab_groups.clear();
        }
        let session = Session {
            urls: ordered.iter().map(|tab| tab.uri.borrow().clone()).collect(),
            selected: self.notebook.current_page().unwrap_or(0),
            groups: self.groups.groups.borrow().clone(),
            tab_groups,
            appearance: self.appearance.get(),
        };
        if let Err(error) = session.save() {
            eprintln!("Could not save session: {error}");
        }
    }
}

fn build_window(app: &gtk::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Tillwind")
        .default_width(1080)
        .default_height(720)
        .build();
    let header = gtk::HeaderBar::new();
    header.add_css_class("browser-toolbar");
    let address = gtk::Entry::builder()
        .placeholder_text("Search or enter website name")
        .width_chars(36)
        .max_width_chars(42)
        .halign(gtk::Align::Center)
        .build();
    address.update_property(&[gtk::accessible::Property::Label("Address and search")]);
    address.add_css_class("browser-address");
    address.set_primary_icon_name(Some("edit-find-symbolic"));
    address.set_primary_icon_activatable(false);
    address.set_secondary_icon_name(Some("view-refresh-symbolic"));
    address.set_secondary_icon_tooltip_text(Some("Reload page"));
    header.set_title_widget(Some(&address));
    let back = icon("go-previous-symbolic", "Back");
    let forward = icon("go-next-symbolic", "Forward");
    let add = icon("list-add-symbolic", "New tab");
    let groups_menu = gtk::MenuButton::new();
    groups_menu.add_css_class("toolbar-pill");
    groups_menu.set_always_show_arrow(true);
    groups_menu.set_tooltip_text(Some("Tab Groups (Ctrl+Shift+G)"));
    let group_label = gtk::Label::new(Some("Tabs"));
    group_label.set_max_width_chars(12);
    group_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let group_button = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    group_button.append(&gtk::Image::from_icon_name("view-list-symbolic"));
    group_button.append(&group_label);
    groups_menu.set_child(Some(&group_button));
    let groups_popover = gtk::Popover::new();
    groups_menu.set_popover(Some(&groups_popover));
    header.pack_start(&groups_menu);
    let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    navigation.add_css_class("toolbar-pill");
    navigation.append(&back);
    navigation.append(&forward);
    header.pack_start(&navigation);

    let monitor = gtk::ToggleButton::new();
    monitor.set_child(Some(&performance::gauge_icon()));
    monitor.add_css_class("flat");
    monitor.add_css_class("performance-toggle");
    monitor.set_tooltip_text(Some("Performance sidebar (Ctrl+Shift+P)"));
    monitor.update_property(&[gtk::accessible::Property::Label("Performance sidebar")]);
    let appearance_menu = gtk::MenuButton::new();
    appearance_menu.set_icon_name("display-brightness-symbolic");
    appearance_menu.set_tooltip_text(Some("Appearance"));
    appearance_menu.add_css_class("flat");
    let appearances = gio::Menu::new();
    for (label, value) in [("System", "system"), ("Light", "light"), ("Dark", "dark")] {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some("win.appearance"), Some(&value.to_variant()));
        appearances.append_item(&item);
    }
    appearance_menu.set_menu_model(Some(&appearances));
    let utilities = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    utilities.add_css_class("toolbar-pill");
    utilities.append(&appearance_menu);
    utilities.append(&monitor);
    utilities.append(&add);
    header.pack_end(&utilities);
    window.set_titlebar(Some(&header));
    let notebook = gtk::Notebook::builder()
        .scrollable(true)
        .show_border(false)
        .build();
    notebook.add_css_class("browser-tabs");
    let performance = performance::Performance::new();
    window.set_child(Some(&notebook));
    let address_focus = gtk::EventControllerFocus::new();
    address.add_controller(address_focus.clone());
    let browser = Rc::new(Browser {
        notebook,
        address,
        address_focus,
        back,
        forward,
        tabs: RefCell::new(Vec::new()),
        performance,
        performance_timer: Default::default(),
        groups: Default::default(),
        group_label,
        appearance: Cell::new(Appearance::System),
    });

    if let Some(data_manager) =
        webkit6::NetworkSession::default().and_then(|session| session.website_data_manager())
    {
        data_manager.set_favicons_enabled(true);
    }
    let session = Session::read();
    browser.groups.groups.replace(session.groups);
    browser.appearance.set(session.appearance);
    session.appearance.apply(&window);
    let appearance_action = gio::SimpleAction::new_stateful(
        "appearance",
        Some(glib::VariantTy::STRING),
        &session.appearance.value().to_variant(),
    );
    appearance_action.connect_activate(glib::clone!(
        #[weak]
        browser,
        #[weak]
        window,
        move |action, value| {
            let appearance = match value.and_then(glib::Variant::str) {
                Some("system") => Appearance::System,
                Some("light") => Appearance::Light,
                Some("dark") => Appearance::Dark,
                _ => return,
            };
            browser.appearance.set(appearance);
            appearance.apply(&window);
            action.set_state(&appearance.value().to_variant());
        }
    ));
    window.add_action(&appearance_action);
    let mut selected = None;
    for (index, uri) in session.urls.into_iter().enumerate() {
        if uri == BLANK || web_uri(&uri).is_some() {
            let group = session
                .tab_groups
                .get(index)
                .copied()
                .flatten()
                .filter(|id| {
                    browser
                        .groups
                        .groups
                        .borrow()
                        .iter()
                        .any(|group| group.id == *id)
                });
            let page = browser.add_tab(uri, group);
            if index == session.selected as usize {
                selected = browser
                    .notebook
                    .nth_page(Some(page))
                    .and_then(|page| browser.tab_for(&page));
            }
        }
    }
    let selected = selected.or_else(|| browser.current());
    let group = selected.as_ref().and_then(|tab| tab.group.get());
    browser.show_group(group, selected);
    browser.notebook.connect_switch_page(glib::clone!(
        #[weak]
        browser,
        move |_, page, _| {
            if !browser.groups.changing.get()
                && let Some(tab) = browser.tab_for(page)
            {
                browser.activate_tab(&tab);
                browser.refresh_performance();
            }
        }
    ));
    groups_popover.connect_show(glib::clone!(
        #[weak]
        browser,
        move |popover| groups::populate(&browser, popover)
    ));
    let open_groups = gio::SimpleAction::new("open-groups", None);
    open_groups.connect_activate(glib::clone!(
        #[weak]
        groups_menu,
        move |_, _| groups_menu.popup()
    ));
    window.add_action(&open_groups);
    app.set_accels_for_action("win.open-groups", &["<Control><Shift>g"]);

    browser.address_focus.connect_enter(glib::clone!(
        #[weak]
        browser,
        move |_| {
            if let Some(tab) = browser.current() {
                browser.sync_address(&tab, true);
            }
        }
    ));
    browser.address_focus.connect_leave(glib::clone!(
        #[weak]
        browser,
        move |_| {
            if let Some(tab) = browser.current() {
                browser.sync_address(&tab, false);
            }
        }
    ));
    browser.address.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |entry| {
            if let (Some(tab), Some(uri)) = (browser.current(), address_target(&entry.text())) {
                if let Some(view) = tab.view.borrow().clone() {
                    view.load_uri(&uri);
                    view.grab_focus();
                }
            }
        }
    ));
    for (button, navigate) in [
        (&browser.back, WebView::go_back as fn(&WebView)),
        (&browser.forward, WebView::go_forward as fn(&WebView)),
    ] {
        button.connect_clicked(glib::clone!(
            #[weak]
            browser,
            move |_| {
                if let Some(view) = browser.current().and_then(|tab| tab.view.borrow().clone()) {
                    navigate(&view);
                }
            }
        ));
    }
    add.connect_clicked(glib::clone!(
        #[weak]
        browser,
        move |_| browser.new_tab()
    ));

    monitor.connect_toggled(glib::clone!(
        #[weak]
        browser,
        move |button| {
            browser.performance.widget.set_visible(button.is_active());
            if button.is_active() {
                browser.refresh_performance();
                let browser_ref = Rc::downgrade(&browser);
                let timer = glib::timeout_add_local(Duration::from_secs(1), move || {
                    let Some(browser) = browser_ref.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    browser.refresh_performance();
                    glib::ControlFlow::Continue
                });
                browser.performance_timer.replace(Some(timer));
            } else {
                if let Some(timer) = browser.performance_timer.take() {
                    timer.remove();
                }
                browser.performance.reset();
            }
        }
    ));
    let toggle_performance = gio::SimpleAction::new("toggle-performance", None);
    toggle_performance.connect_activate(glib::clone!(
        #[weak]
        monitor,
        move |_, _| monitor.set_active(!monitor.is_active())
    ));
    window.add_action(&toggle_performance);
    app.set_accels_for_action("win.toggle-performance", &["<Control><Shift>p"]);
    let reload = gio::SimpleAction::new("reload", None);
    reload.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |_, _| {
            if let Some(view) = browser.current().and_then(|tab| tab.view.borrow().clone()) {
                view.reload();
            }
        }
    ));
    window.add_action(&reload);
    browser.address.connect_icon_release(glib::clone!(
        #[weak]
        reload,
        move |_, position| {
            if position == gtk::EntryIconPosition::Secondary {
                reload.activate(None);
            }
        }
    ));

    let new_tab = gio::SimpleAction::new("new-tab", None);
    new_tab.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |_, _| browser.new_tab()
    ));
    window.add_action(&new_tab);
    let close_tab = gio::SimpleAction::new("close-tab", None);
    close_tab.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |_, _| {
            if let Some(tab) = browser.current() {
                browser.close_tab(&tab);
            }
        }
    ));
    window.add_action(&close_tab);
    let focus_address = gio::SimpleAction::new("focus-address", None);
    focus_address.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |_, _| {
            browser.address.grab_focus();
            browser.address.select_region(0, -1);
        }
    ));
    window.add_action(&focus_address);
    let select_tab = gio::SimpleAction::new("select-tab", Some(glib::VariantTy::UINT32));
    select_tab.connect_activate(glib::clone!(
        #[weak]
        browser,
        move |_, parameter| {
            if let Some(index) = parameter.and_then(glib::Variant::get::<u32>)
                && let Some(tab) = browser
                    .tabs_in_group(browser.groups.active.get())
                    .get(index as usize)
            {
                browser
                    .notebook
                    .set_current_page(browser.notebook.page_num(&tab.page));
            }
        }
    ));
    window.add_action(&select_tab);
    for index in 0..9u32 {
        let action = gio::Action::print_detailed_name("win.select-tab", Some(&index.to_variant()));
        app.set_accels_for_action(&action, &[&format!("<Control>{}", index + 1)]);
    }
    app.set_accels_for_action("win.reload", &["<Control>r"]);
    app.set_accels_for_action("win.new-tab", &["<Control>t"]);
    app.set_accels_for_action("win.close-tab", &["<Control>w"]);
    app.set_accels_for_action("win.focus-address", &["<Control>l"]);
    window.connect_close_request(move |_| {
        if let Some(timer) = browser.performance_timer.take() {
            timer.remove();
        }
        browser.performance.reset();
        browser.save();
        glib::Propagation::Proceed
    });
    window.present();
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    let extension = match performance::WebExtension::install() {
        Ok(extension) => Some(Rc::new(extension)),
        Err(error) => {
            eprintln!("Tab performance extension unavailable: {error}");
            None
        }
    };
    app.connect_startup(move |_| {
        if let Some(extension) = &extension
            && let Some(context) = webkit6::WebContext::default()
        {
            extension.configure(&context);
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_data(include_str!("browser.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(build_window);
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_from_before_tab_groups_remain_compatible() {
        let session: Session =
            serde_json::from_str(r#"{"urls":["https://example.com/"],"selected":0}"#).unwrap();
        assert!(session.groups.is_empty());
        assert!(session.tab_groups.is_empty());
        assert!(session.appearance.is_system());
        assert_eq!(
            serde_json::to_value(session).unwrap(),
            serde_json::json!({"urls":["https://example.com/"],"selected":0})
        );
    }

    #[test]
    fn group_membership_and_appearance_survive_serialization() {
        let value = serde_json::json!({
            "urls": ["https://example.com/", "about:blank"],
            "selected": 1,
            "groups": [{"id": 7, "name": "Research"}],
            "tab_groups": [null, 7],
            "appearance": "dark"
        });
        let session: Session = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(session).unwrap(), value);
    }
}
