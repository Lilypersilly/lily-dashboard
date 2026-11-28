// @dash-id: calculator
// @dash-title: Calculator
// @dash-lifecycle: stateful
//! Steins;Gate Divergence & Valalieh Base-6 Calculator Module

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Entry, Grid, Label, Orientation, PolicyType, ScrolledWindow};
use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

// --- MATH & PARSER ENGINE ---

fn eval_expr(input: &str, base: u32) -> Result<f64, String> {
    let tokens = tokenize(input, base)?;
    if tokens.is_empty() {
        return Err("Empty".to_string());
    }
    let mut pos = 0usize;
    let val = parse_add(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err("Syntax error".to_string());
    }
    if !val.is_finite() {
        return Err("Math error".to_string());
    }
    Ok(val)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Op(char),
    LParen,
    RParen,
}

fn tokenize(s: &str, base: u32) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let is_valid_digit = if base == 10 {
            c.is_ascii_digit() || c == '.'
        } else {
            c.is_digit(base) // Base-6 or Base-2
        };

        if is_valid_digit {
            let start = i;
            while i < chars.len() && (chars[i].is_digit(base) || (base == 10 && chars[i] == '.')) {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            let n = if base == 10 {
                num_str.parse::<f64>().map_err(|_| "Invalid number".to_string())?
            } else {
                i64::from_str_radix(&num_str, base).map_err(|_| "Invalid base".to_string())? as f64
            };
            out.push(Tok::Num(n));
            continue;
        }
        match c {
            '+' | '-' | '*' | '/' | '%' | '^' => out.push(Tok::Op(c)),
            '×' => out.push(Tok::Op('*')),
            '÷' => out.push(Tok::Op('/')),
            '(' => out.push(Tok::LParen),
            ')' => out.push(Tok::RParen),
            _ => return Err(format!("Unexpected '{}'", c)),
        }
        i += 1;
    }
    Ok(out)
}

fn parse_add(toks: &[Tok], pos: &mut usize) -> Result<f64, String> {
    let mut lhs = parse_mul(toks, pos)?;
    while let Some(Tok::Op(op @ ('+' | '-'))) = toks.get(*pos) {
        let op = *op;
        *pos += 1;
        let rhs = parse_mul(toks, pos)?;
        lhs = if op == '+' { lhs + rhs } else { lhs - rhs };
    }
    Ok(lhs)
}

fn parse_mul(toks: &[Tok], pos: &mut usize) -> Result<f64, String> {
    let mut lhs = parse_pow(toks, pos)?;
    while let Some(Tok::Op(op @ ('*' | '/' | '%'))) = toks.get(*pos) {
        let op = *op;
        *pos += 1;
        let rhs = parse_pow(toks, pos)?;
        lhs = match op {
            '*' => lhs * rhs,
            '/' => {
                if rhs == 0.0 {
                    return Err("Divide by zero".to_string());
                }
                lhs / rhs
            }
            _ => lhs % rhs,
        };
    }
    Ok(lhs)
}

fn parse_pow(toks: &[Tok], pos: &mut usize) -> Result<f64, String> {
    let base = parse_unary(toks, pos)?;
    if let Some(Tok::Op('^')) = toks.get(*pos) {
        *pos += 1;
        let exp = parse_pow(toks, pos)?;
        Ok(base.powf(exp))
    } else {
        Ok(base)
    }
}

fn parse_unary(toks: &[Tok], pos: &mut usize) -> Result<f64, String> {
    match toks.get(*pos) {
        Some(Tok::Op('-')) => {
            *pos += 1;
            Ok(-parse_unary(toks, pos)?)
        }
        Some(Tok::Op('+')) => {
            *pos += 1;
            parse_unary(toks, pos)
        }
        _ => parse_primary(toks, pos),
    }
}

fn parse_primary(toks: &[Tok], pos: &mut usize) -> Result<f64, String> {
    match toks.get(*pos) {
        Some(Tok::Num(n)) => {
            *pos += 1;
            Ok(*n)
        }
        Some(Tok::LParen) => {
            *pos += 1;
            let val = parse_add(toks, pos)?;
            if toks.get(*pos) == Some(&Tok::RParen) {
                *pos += 1;
                Ok(val)
            } else {
                Err("Missing ')'".to_string())
            }
        }
        _ => Err("Expected number".to_string()),
    }
}

fn format_base(v: f64, base: u32) -> String {
    if base == 10 {
        if (v - v.round()).abs() < 1e-10 && v.abs() < 1e15 {
            format!("{}", v.round() as i64)
        } else {
            let s = format!("{:.6}", v);
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
    } else {
        let mut int_val = v.round() as i64;
        let sign = if int_val < 0 { "-" } else { "" };
        int_val = int_val.abs();
        if int_val == 0 { return "0".to_string(); }
        let mut res = String::new();
        while int_val > 0 {
            res.push(std::char::from_digit((int_val % (base as i64)) as u32, base).unwrap());
            int_val /= base as i64;
        }
        format!("{}{}", sign, res.chars().rev().collect::<String>())
    }
}

fn generate_random_spin() -> String {
    let mut buf = [0u8; 4];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut buf);
    }
    let n = u32::from_le_bytes(buf) % 1000000;
    format!("1.{:06}", n)
}

// --- UI BUILDER ---

pub fn build(_ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
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
            .label("DIVERGENCE CALCULATOR")
            .halign(Align::Start)
            .css_classes(vec!["fav-group-header".to_string()])
            .build(),
    );

    // BASE TOGGLE ROW
    let base_row = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let current_base = Rc::new(Cell::new(10u32));
    
    let btn_dec = Button::builder().label("DEC").css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()]).hexpand(true).build();
    let btn_bin = Button::builder().label("BIN").css_classes(vec!["plan-fmt-btn".to_string()]).hexpand(true).build();
    let btn_val = Button::builder().label("VALALIEH").css_classes(vec!["plan-fmt-btn".to_string()]).hexpand(true).build();
    
    base_row.append(&btn_dec);
    base_row.append(&btn_bin);
    base_row.append(&btn_val);
    root.append(&base_row);

    let disp_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let expr_entry = Entry::builder()
        .placeholder_text("Equation...")
        .hexpand(true)
        .build();

    let result_btn = Button::builder()
        .label("[ 0.000000 ]")
        .halign(Align::End)
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Click to copy result")
        .build();

    disp_card.append(&expr_entry);
    disp_card.append(&result_btn);
    root.append(&disp_card);

    let grid = Grid::builder()
        .row_spacing(6)
        .column_spacing(6)
        .row_homogeneous(true)
        .column_homogeneous(true)
        .build();

    let keys: [[&str; 4]; 5] = [
        ["C", "(", ")", "☢"],
        ["7", "8", "9", "/"],
        ["4", "5", "6", "*"],
        ["1", "2", "3", "-"],
        ["0", ".", "=", "+"],
    ];

    let mut num_buttons: Vec<(String, Button)> = Vec::new();
    let last_answer = Rc::new(RefCell::new(0.0f64));
    let is_animating = Rc::new(Cell::new(false));

    // Evaluation Closure
    let commit_eval: Rc<dyn Fn()> = {
        let e = expr_entry.clone();
        let rb = result_btn.clone();
        let base_c = current_base.clone();
        let anim_c = is_animating.clone();
        let last_ans_c = last_answer.clone();

        Rc::new(move || {
            if anim_c.get() { return; }
            let expr = e.text().trim().to_string();
            if expr.is_empty() { return; }
            
            match eval_expr(&expr, base_c.get()) {
                Ok(val) => {
                    *last_ans_c.borrow_mut() = val;
                    anim_c.set(true);
                    rb.remove_css_class("plan-line-del-btn"); // Remove quantum red
                    
                    let ticks = Rc::new(Cell::new(0));
                    let rb_anim = rb.clone();
                    let base_anim = base_c.clone();
                    let anim_state = anim_c.clone();
                    let e_anim = e.clone();

                    // Steins;Gate Divergence Spin Animation
                    glib::timeout_add_local(Duration::from_millis(40), move || {
                        ticks.set(ticks.get() + 1);
                        if ticks.get() < 10 {
                            rb_anim.set_label(&format!("[ {} ]", generate_random_spin()));
                            glib::ControlFlow::Continue
                        } else {
                            let ans_str = format_base(val, base_anim.get());
                            rb_anim.set_label(&format!("[ {} ]", ans_str));
                            e_anim.set_text(&ans_str);
                            e_anim.set_position(-1);
                            anim_state.set(false);
                            glib::ControlFlow::Break
                        }
                    });
                }
                Err(msg) => { rb.set_label(&format!("[ {} ]", msg)); }
            }
        })
    };

    // Quantum Decoherence Action
    let trigger_quantum: Rc<dyn Fn()> = {
        let rb = result_btn.clone();
        let e = expr_entry.clone();
        let anim_c = is_animating.clone();
        let base_c = current_base.clone();
        
        Rc::new(move || {
            if anim_c.get() { return; }
            let expr = e.text().trim().to_string();
            if expr.is_empty() { return; }
            
            let current_val = match eval_expr(&expr, base_c.get()) {
                Ok(v) => v,
                Err(_) => return,
            };

            anim_c.set(true);
            rb.add_css_class("plan-line-del-btn"); // Red Alert CSS
            
            let (tx, rx) = std::sync::mpsc::channel::<f64>();
            std::thread::spawn(move || {
                let url = "https://lizaonair.com/qrng/?min=1&max=99";
                if let Ok(out) = Command::new("curl").args(["-sL", "-H", "Accept: application/json", "--max-time", "2", url]).output() {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                        if let Some(num) = json.get("value").and_then(|v| v.as_f64()) {
                            let _ = tx.send(num);
                            return;
                        }
                    }
                }
                let _ = tx.send(42.0); // Fallback quantum drift
            });

            let ticks = Rc::new(Cell::new(0));
            let rb_anim = rb.clone();
            let e_anim = e.clone();
            let anim_state = anim_c.clone();
            let base_anim = base_c.clone();

            glib::timeout_add_local(Duration::from_millis(40), move || {
                ticks.set(ticks.get() + 1);
                rb_anim.set_label(&format!("[ {} ]", generate_random_spin()));
                
                if let Ok(q_num) = rx.try_recv() {
                    let new_val = current_val * q_num;
                    let ans_str = format_base(new_val, base_anim.get());
                    rb_anim.set_label(&format!("[ {} ]", ans_str));
                    e_anim.set_text(&ans_str);
                    e_anim.set_position(-1);
                    anim_state.set(false);
                    return glib::ControlFlow::Break;
                }
                if ticks.get() > 50 { 
                    anim_state.set(false); 
                    return glib::ControlFlow::Break; 
                }
                glib::ControlFlow::Continue
            });
        })
    };

    // Button Grid Setup
    for (r, row) in keys.iter().enumerate() {
        for (c, &label) in row.iter().enumerate() {
            let cls = if label == "=" || label == "☢" {
                vec!["primary-btn".to_string()]
            } else {
                vec!["fav-item-btn".to_string()]
            };
            let btn = Button::builder().label(label).css_classes(cls).hexpand(true).build();

            let e = expr_entry.clone();
            let ce = commit_eval.clone();
            let cq = trigger_quantum.clone();
            let k = label.to_string();
            
            btn.connect_clicked(move |_| match k.as_str() {
                "C" => e.set_text(""),
                "☢" => cq(),
                "⌫" => {
                    let mut s = e.text().to_string();
                    s.pop();
                    e.set_text(&s);
                    e.set_position(-1);
                }
                "=" => ce(),
                other => {
                    let mut s = e.text().to_string();
                    s.push_str(other);
                    e.set_text(&s);
                    e.set_position(-1);
                }
            });
            grid.attach(&btn, c as i32, r as i32, 1, 1);
            num_buttons.push((label.to_string(), btn));
        }
    }
    root.append(&grid);

    // BASE TOGGLE LOGIC
    let num_btns_rc = Rc::new(RefCell::new(num_buttons));
    let update_base = {
        let btns = num_btns_rc.clone();
        Rc::new(move |base: u32| {
            for (lbl, btn) in btns.borrow().iter() {
                let sensitive = match lbl.as_str() {
                    "6" | "7" | "8" | "9" => base == 10,
                    "2" | "3" | "4" | "5" => base != 2,
                    "." => base == 10,
                    _ => true,
                };
                btn.set_sensitive(sensitive);
            }
        })
    };

    {
        let bd = btn_dec.clone();
        let bb = btn_bin.clone();
        let bv = btn_val.clone();
        let cb = current_base.clone();
        let ub = update_base.clone();
        btn_dec.connect_clicked(move |_| {
            bd.add_css_class("active"); bb.remove_css_class("active"); bv.remove_css_class("active");
            cb.set(10); ub(10);
        });
    }
    {
        let bd = btn_dec.clone();
        let bb = btn_bin.clone();
        let bv = btn_val.clone();
        let cb = current_base.clone();
        let ub = update_base.clone();
        btn_bin.connect_clicked(move |_| {
            bb.add_css_class("active"); bd.remove_css_class("active"); bv.remove_css_class("active");
            cb.set(2); ub(2);
        });
    }
    {
        let bd = btn_dec.clone();
        let bb = btn_bin.clone();
        let bv = btn_val.clone();
        let cb = current_base.clone();
        let ub = update_base.clone();
        btn_val.connect_clicked(move |_| {
            bv.add_css_class("active"); bd.remove_css_class("active"); bb.remove_css_class("active");
            cb.set(6); ub(6);
        });
    }

    let entry_focus = expr_entry.clone();
    crate::module_api::ModuleHandle {
        widget: root.upcast(),
        detail_pane: None,
        on_show: Some(Rc::new(move || {
            entry_focus.grab_focus();
        })),
        on_hide: None,
    }
}