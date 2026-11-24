use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, Calendar, Entry, Label, Orientation, PolicyType, ScrolledWindow,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Serialize, Deserialize, Default, Clone)]
struct CalendarStore {
    /// Map of "YYYY-MM-DD" -> list of reminder/event strings
    #[serde(default)]
    events: BTreeMap<String, Vec<String>>,
}

fn events_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("calendar_events.json");
    path
}

fn load_store() -> CalendarStore {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    path.push("calendar_events.json");
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(st) = serde_json::from_str::<CalendarStore>(&data) {
            return st;
        }
    }
    CalendarStore::default()
}

fn save_store(store: &CalendarStore) {
    if let Ok(json) = serde_json::to_string_pretty(store) {
        let _ = fs::write(events_path(), json);
    }
}

fn date_key(dt: &glib::DateTime) -> String {
    format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day_of_month())
}

fn pretty_date(dt: &glib::DateTime) -> String {
    dt.format("%a, %d %b %Y")
        .map(|s| s.to_string())
        .unwrap_or_else(|_| date_key(dt))
}

fn mark_month_days(cal: &Calendar, store: &CalendarStore) {
    cal.clear_marks();
    let dt = cal.date();
    let prefix = format!("{:04}-{:02}-", dt.year(), dt.month());
    for (k, items) in &store.events {
        if !items.is_empty() {
            if let Some(day_str) = k.strip_prefix(&prefix) {
                if let Ok(day) = day_str.parse::<u32>() {
                    cal.mark_day(day);
                }
            }
        }
    }
}

pub fn build_calendar_ui() -> Box {
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let hdr_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let today_str = glib::DateTime::now_local()
        .map(|dt| pretty_date(&dt))
        .unwrap_or_else(|_| "CALENDAR".to_string());

    let hdr_lbl = Label::builder()
        .label(&today_str.to_uppercase())
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let today_btn = Button::builder()
        .label("Today")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();

    hdr_row.append(&hdr_lbl);
    hdr_row.append(&today_btn);
    root.append(&hdr_row);

    let cal = Calendar::builder()
        .show_heading(true)
        .show_day_names(true)
        .show_week_numbers(false)
        .css_classes(vec!["lily-calendar".to_string()])
        .build();
    root.append(&cal);

    let selected_lbl = Label::builder()
        .label(&pretty_date(&cal.date()))
        .halign(Align::Start)
        .css_classes(vec!["plan-step".to_string()])
        .margin_top(4)
        .build();
    root.append(&selected_lbl);

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    let events_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .build();
    scroll.set_child(Some(&events_box));
    root.append(&scroll);

    let add_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let add_entry = Entry::builder()
        .placeholder_text("Add note / event for day...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();
    let add_btn = Button::builder()
        .label("+")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    add_row.append(&add_entry);
    add_row.append(&add_btn);
    root.append(&add_row);

    let store_rc = Rc::new(RefCell::new(load_store()));

    let render_day: Rc<dyn Fn()> = {
        let cal_r = cal.clone();
        let lbl_r = selected_lbl.clone();
        let box_r = events_box.clone();
        let st_r = store_rc.clone();
        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_slot.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            let dt = cal_r.date();
            let key = date_key(&dt);
            lbl_r.set_label(&format!("NOTES • {}", pretty_date(&dt)));
            mark_month_days(&cal_r, &st_r.borrow());

            while let Some(child) = box_r.first_child() {
                box_r.remove(&child);
            }

            let items = st_r
                .borrow()
                .events
                .get(&key)
                .cloned()
                .unwrap_or_default();

            if items.is_empty() {
                let empty = Label::builder()
                    .label("No notes for this day.")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build();
                box_r.append(&empty);
            } else {
                for (idx, text) in items.into_iter().enumerate() {
                    let row = Box::builder()
                        .orientation(Orientation::Horizontal)
                        .spacing(6)
                        .css_classes(vec!["plan-check-row".to_string()])
                        .build();
                    let txt_lbl = Label::builder()
                        .label(&text)
                        .halign(Align::Start)
                        .hexpand(true)
                        .wrap(true)
                        .xalign(0.0)
                        .css_classes(vec!["plan-body".to_string()])
                        .build();
                    let del_btn = Button::builder()
                        .label("x")
                        .css_classes(vec!["plan-line-del-btn".to_string()])
                        .valign(Align::Center)
                        .build();

                    let key_d = key.clone();
                    let st_d = st_r.clone();
                    let rerender = self_weak.clone();
                    del_btn.connect_clicked(move |_| {
                        {
                            let mut st = st_d.borrow_mut();
                            if let Some(list) = st.events.get_mut(&key_d) {
                                if idx < list.len() {
                                    list.remove(idx);
                                }
                                if list.is_empty() {
                                    st.events.remove(&key_d);
                                }
                            }
                            save_store(&st);
                        }
                        if let Some(ref f) = *rerender.borrow() {
                            f();
                        }
                    });

                    row.append(&txt_lbl);
                    row.append(&del_btn);
                    box_r.append(&row);
                }
            }
        });

        *self_slot.borrow_mut() = Some(cb.clone());
        cb
    };

    render_day();

    {
        let r = render_day.clone();
        cal.connect_day_selected(move |_| r());
    }
    {
        let r = render_day.clone();
        cal.connect_next_month(move |_| r());
    }
    {
        let r = render_day.clone();
        cal.connect_prev_month(move |_| r());
    }
    {
        let r = render_day.clone();
        cal.connect_next_year(move |_| r());
    }
    {
        let r = render_day.clone();
        cal.connect_prev_year(move |_| r());
    }

    {
        let cal_t = cal.clone();
        let r = render_day.clone();
        today_btn.connect_clicked(move |_| {
            if let Ok(now) = glib::DateTime::now_local() {
                cal_t.select_day(&now);
                r();
            }
        });
    }

    let add_event = {
        let cal_a = cal.clone();
        let entry_a = add_entry.clone();
        let st_a = store_rc.clone();
        let r = render_day.clone();
        Rc::new(move || {
            let txt = entry_a.text().trim().to_string();
            if txt.is_empty() {
                return;
            }
            let key = date_key(&cal_a.date());
            {
                let mut st = st_a.borrow_mut();
                st.events.entry(key).or_default().push(txt);
                save_store(&st);
            }
            entry_a.set_text("");
            r();
        })
    };

    {
        let ae = add_event.clone();
        add_btn.connect_clicked(move |_| ae());
    }
    {
        let ae = add_event.clone();
        add_entry.connect_activate(move |_| ae());
    }

    root
}
