use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

use gtk::{glib, prelude::*};
use serde::{Deserialize, Serialize};

use crate::{BLANK, Browser, Tab, icon};

#[derive(Clone, Deserialize, Serialize)]
pub struct Group {
    pub id: u64,
    pub name: String,
}

#[derive(Default)]
pub struct TabGroups {
    pub groups: RefCell<Vec<Group>>,
    pub active: Cell<Option<u64>>,
    pub changing: Cell<bool>,
    selected: RefCell<HashMap<Option<u64>, Weak<Tab>>>,
}

impl Browser {
    pub fn tabs_in_group(&self, group: Option<u64>) -> Vec<Rc<Tab>> {
        (0..self.notebook.n_pages())
            .filter_map(|index| self.notebook.nth_page(Some(index)))
            .filter_map(|page| self.tab_for(&page))
            .filter(|tab| tab.group.get() == group)
            .collect()
    }

    pub fn show_group(self: &Rc<Self>, group: Option<u64>, selected: Option<Rc<Tab>>) {
        let old_group = self.groups.active.get();
        if let Some(tab) = self.current()
            && tab.group.get() == old_group
        {
            self.groups
                .selected
                .borrow_mut()
                .insert(old_group, Rc::downgrade(&tab));
        }
        self.groups.changing.set(true);
        self.groups.active.set(group);
        let mut tabs = self.tabs_in_group(group);
        if tabs.is_empty() {
            self.add_tab(BLANK.into(), group);
            tabs = self.tabs_in_group(group);
        }
        let previous = self
            .groups
            .selected
            .borrow()
            .get(&group)
            .and_then(Weak::upgrade);
        let selected = selected
            .or(previous)
            .filter(|tab| tabs.iter().any(|candidate| Rc::ptr_eq(candidate, tab)))
            .unwrap_or_else(|| tabs[0].clone());

        for tab in &tabs {
            tab.page.set_visible(true);
        }
        self.notebook
            .set_current_page(self.notebook.page_num(&selected.page));
        for tab in self.tabs.borrow().iter() {
            if tab.group.get() != group {
                tab.page.set_visible(false);
            }
        }
        self.groups.changing.set(false);
        self.update_group_label();
        self.activate_tab(&selected);
        self.refresh_performance();
    }

    fn update_group_label(&self) {
        let groups = self.groups.groups.borrow();
        let name = groups
            .iter()
            .find(|group| Some(group.id) == self.groups.active.get())
            .map_or("Tabs", |group| group.name.as_str());
        self.group_label.set_text(name);
        self.group_label.set_tooltip_text(Some(name));
    }

    fn move_current_tab(self: &Rc<Self>, group: Option<u64>) {
        if let Some(tab) = self.current() {
            tab.group.set(group);
            self.show_group(group, Some(tab));
        }
    }

    fn group_name(&self, input: &str, except: Option<u64>) -> Result<String, &'static str> {
        let name = input.trim();
        if name.is_empty() {
            return Err("Enter a group name.");
        }
        if name.eq_ignore_ascii_case("Ungrouped")
            || self.groups.groups.borrow().iter().any(|group| {
                Some(group.id) != except && group.name.to_lowercase() == name.to_lowercase()
            })
        {
            return Err("That group name is already in use.");
        }
        Ok(name.to_owned())
    }

    fn create_group(self: &Rc<Self>, input: &str) -> Result<(), &'static str> {
        let name = self.group_name(input, None)?;
        let mut groups = self.groups.groups.borrow_mut();
        let id = (1..)
            .find(|id| !groups.iter().any(|group| group.id == *id))
            .unwrap();
        groups.push(Group { id, name });
        drop(groups);
        self.move_current_tab(Some(id));
        Ok(())
    }

    fn rename_group(&self, id: u64, input: &str) -> Result<(), &'static str> {
        let name = self.group_name(input, Some(id))?;
        if let Some(group) = self
            .groups
            .groups
            .borrow_mut()
            .iter_mut()
            .find(|group| group.id == id)
        {
            group.name = name;
        }
        self.update_group_label();
        Ok(())
    }

    fn remove_group(self: &Rc<Self>, id: u64) {
        let selected = self.current();
        for tab in self.tabs_in_group(Some(id)) {
            tab.group.set(None);
        }
        self.groups
            .groups
            .borrow_mut()
            .retain(|group| group.id != id);
        self.groups.selected.borrow_mut().remove(&Some(id));
        self.show_group(None, selected);
    }
}

pub fn populate(browser: &Rc<Browser>, popover: &gtk::Popover) {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    panel.add_css_class("tab-groups-menu");
    let heading = gtk::Label::new(Some("Tab Groups"));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    panel.append(&heading);

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let groups = browser.groups.groups.borrow();
    for (id, name) in std::iter::once((None, "Ungrouped")).chain(
        groups
            .iter()
            .map(|group| (Some(group.id), group.name.as_str())),
    ) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let selected = browser.groups.active.get() == id;
        let open = gtk::Button::new();
        open.add_css_class("flat");
        if selected {
            open.add_css_class("current-group");
        }
        let text = gtk::Label::new(Some(name));
        text.set_xalign(0.0);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        text.set_max_width_chars(22);
        let item = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        let status = gtk::Image::from_icon_name("object-select-symbolic");
        status.set_opacity(if selected { 1.0 } else { 0.0 });
        item.append(&status);
        item.append(&text);
        text.set_hexpand(true);
        let count = gtk::Label::new(Some(&browser.tabs_in_group(id).len().to_string()));
        count.add_css_class("dim-label");
        item.append(&count);
        open.set_child(Some(&item));
        open.set_tooltip_text(Some(name));
        open.set_hexpand(true);
        open.connect_clicked(glib::clone!(
            #[weak]
            browser,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                browser.show_group(id, None);
            }
        ));
        let move_tab = icon("go-jump-symbolic", &format!("Move current tab to {name}"));
        move_tab.set_sensitive(!selected);
        move_tab.connect_clicked(glib::clone!(
            #[weak]
            browser,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                browser.move_current_tab(id);
            }
        ));
        row.append(&open);
        row.append(&move_tab);
        rows.append(&row);
    }
    drop(groups);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(220)
        .propagate_natural_height(true)
        .child(&rows)
        .build();
    panel.append(&scroller);

    panel.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let name = gtk::Entry::builder()
        .placeholder_text("Group name")
        .max_length(64)
        .build();
    panel.append(&name);
    let error = gtk::Label::new(None);
    error.set_xalign(0.0);
    error.add_css_class("error");
    error.set_visible(false);
    panel.append(&error);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    let create = gtk::Button::with_label("Group This Tab");
    create.connect_clicked(glib::clone!(
        #[weak]
        browser,
        #[weak]
        popover,
        #[weak]
        name,
        #[weak]
        error,
        move |_| match browser.create_group(&name.text()) {
            Ok(()) => popover.popdown(),
            Err(message) => {
                error.set_text(message);
                error.set_visible(true);
            }
        }
    ));
    name.connect_activate(glib::clone!(
        #[weak]
        create,
        move |_| create.emit_clicked()
    ));
    actions.append(&create);
    if let Some(id) = browser.groups.active.get() {
        let rename = gtk::Button::with_label("Rename");
        rename.connect_clicked(glib::clone!(
            #[weak]
            browser,
            #[weak]
            popover,
            #[weak]
            name,
            #[weak]
            error,
            move |_| match browser.rename_group(id, &name.text()) {
                Ok(()) => popover.popdown(),
                Err(message) => {
                    error.set_text(message);
                    error.set_visible(true);
                }
            }
        ));
        actions.append(&rename);
        let remove = gtk::Button::with_label("Remove Group");
        remove.set_tooltip_text(Some("Move its tabs to Ungrouped and remove the group"));
        remove.connect_clicked(glib::clone!(
            #[weak]
            browser,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                browser.remove_group(id);
            }
        ));
        panel.append(&actions);
        panel.append(&remove);
    } else {
        panel.append(&actions);
    }
    popover.set_child(Some(&panel));
}
