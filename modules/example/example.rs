// @dash-id: example
// @dash-title: Example
// @dash-lifecycle: stateful
// @dash-hooks: init
// Dashpendencies:
//! Minimal starter template for building a Lily Dashboard module.
//!
//! To try it out:
//!   1. Copy this file up one directory into `modules/example.rs`
//!   2. Click "Reload Dashboard" in the sidebar footer
//!   3. Confirm the third-party module warning prompt
//!
//! Need external crates? Add them to the `// Dashpendencies:` header above, e.g.:
//!   // Dashpendencies: rand = "0.8", chrono = "0.4"
//!
//! See `GUIDE.txt` in the repo root for the full ModuleContext & CSS reference.

use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Entry, Label, Orientation};
use std::cell::Cell;
use std::rc::Rc;

/// Optional startup hook (enabled by `// @dash-hooks: init` above).
pub fn init() {
    // Runs once when Lily Dashboard starts.
}

/// Required entry point called when your tab is opened.
pub fn build(ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
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

    root.append(
        &Label::builder()
            .label("EXAMPLE MODULE")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .build(),
    );

    let card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let count = Rc::new(Cell::new(0u32));
    let status_lbl = Label::builder()
        .label("Clicks: 0")
        .halign(Align::Start)
        .css_classes(vec!["plan-body".to_string()])
        .build();

    let note_entry = Entry::builder()
        .placeholder_text("Plain Entry widgets auto-match the live theme...")
        .hexpand(true)
        .build();

    let click_btn = Button::builder()
        .label("Increment Counter")
        .css_classes(vec!["primary-btn".to_string()])
        .build();
    {
        let c = count.clone();
        let lbl = status_lbl.clone();
        click_btn.connect_clicked(move |_| {
            let next = c.get() + 1;
            c.set(next);
            lbl.set_label(&format!("Clicks: {}", next));
        });
    }

    let close_btn = Button::builder()
        .label("Close Dashboard")
        .css_classes(vec!["fav-item-btn".to_string()])
        .build();
    {
        let close_cb = ctx.close_drawer.clone();
        close_btn.connect_clicked(move |_| close_cb());
    }

    card.append(&status_lbl);
    card.append(&note_entry);
    card.append(&click_btn);
    card.append(&close_btn);
    root.append(&card);

    let entry_focus = note_entry.clone();
    crate::module_api::ModuleHandle {
        widget: root.upcast(),
        detail_pane: None,
        on_show: Some(Rc::new(move || {
            entry_focus.grab_focus();
        })),
        on_hide: None,
    }
}
