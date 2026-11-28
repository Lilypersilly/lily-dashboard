// @dash-id: scicalc
// @dash-title: SciCalc
// @dash-lifecycle: stateful
// Dashpendencies:
//! Scientific calculator with custom variables, history, settings, and an
//! interactive 400px slide-out Cairo function grapher (pan, zoom, trace, multi-curve).

use gtk4::cairo;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, DrawingArea, Entry, EventControllerMotion, EventControllerScroll,
    EventControllerScrollFlags, GestureDrag, Grid, Label, Orientation, PolicyType, ScrolledWindow,
    Stack,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::f64::consts::{E, PI, TAU};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

// =============================================================================
// SETTINGS & PERSISTENCE (~/.config/lily-dashboard/scicalc.json)
// =============================================================================

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
enum AngleMode {
    Rad,
    Deg,
    Grad,
}

impl Default for AngleMode {
    fn default() -> Self {
        Self::Rad
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
enum NumFormat {
    Normal,
    Sci,
    Eng,
}

impl Default for NumFormat {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct SciCalcSettings {
    #[serde(default)]
    angle_mode: AngleMode,
    #[serde(default)]
    num_format: NumFormat,
    #[serde(default = "default_precision")]
    precision: usize,
    #[serde(default = "default_true")]
    implicit_mul: bool,
    #[serde(default = "default_true")]
    show_grid: bool,
    #[serde(default = "default_true")]
    show_labels: bool,
    #[serde(default = "default_true")]
    show_trace: bool,
    #[serde(default = "default_line_width")]
    curve_width: f64,
    #[serde(default = "default_samples")]
    graph_samples: usize,
    #[serde(default = "default_curves")]
    saved_curves: Vec<String>,
    #[serde(default)]
    variables: BTreeMap<String, f64>,
}

fn default_precision() -> usize {
    6
}
fn default_true() -> bool {
    true
}
fn default_line_width() -> f64 {
    2.2
}
fn default_samples() -> usize {
    600
}
fn default_curves() -> Vec<String> {
    vec!["sin(x)".to_string()]
}

impl Default for SciCalcSettings {
    fn default() -> Self {
        Self {
            angle_mode: AngleMode::Rad,
            num_format: NumFormat::Normal,
            precision: 6,
            implicit_mul: true,
            show_grid: true,
            show_labels: true,
            show_trace: true,
            curve_width: 2.2,
            graph_samples: 600,
            saved_curves: default_curves(),
            variables: BTreeMap::new(),
        }
    }
}

fn config_path() -> PathBuf {
    let mut p = glib::user_config_dir();
    p.push("lily-dashboard");
    let _ = fs::create_dir_all(&p);
    p.push("scicalc.json");
    p
}

fn load_settings() -> SciCalcSettings {
    if let Ok(data) = fs::read_to_string(config_path()) {
        if let Ok(cfg) = serde_json::from_str::<SciCalcSettings>(&data) {
            return cfg;
        }
    }
    SciCalcSettings::default()
}

fn save_settings(cfg: &SciCalcSettings) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), json);
    }
}

// =============================================================================
// MATH PARSER & EVALUATOR (AST-based for instant 60fps graphing)
// =============================================================================

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Num(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Bang,
    LParen,
    RParen,
    Comma,
}

#[derive(Clone, Copy, Debug)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

#[derive(Clone, Debug)]
enum Expr {
    Num(f64),
    Var(String),
    Neg(std::boxed::Box<Expr>),
    Fact(std::boxed::Box<Expr>),
    Bin {
        op: BinOp,
        lhs: std::boxed::Box<Expr>,
        rhs: std::boxed::Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        "sin"
            | "cos"
            | "tan"
            | "csc"
            | "sec"
            | "cot"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "sinh"
            | "cosh"
            | "tanh"
            | "asinh"
            | "acosh"
            | "atanh"
            | "ln"
            | "log"
            | "log2"
            | "log10"
            | "exp"
            | "sqrt"
            | "cbrt"
            | "pow"
            | "abs"
            | "floor"
            | "ceil"
            | "round"
            | "sign"
            | "min"
            | "max"
            | "clamp"
            | "deg"
            | "rad"
            | "gamma"
            | "ncr"
            | "comb"
            | "npr"
            | "perm"
            | "gcd"
            | "lcm"
    )
}

fn tokenize(input: &str, implicit_mul: bool) -> Result<Vec<Token>, String> {
    let cleaned = input
        .replace('×', "*")
        .replace('÷', "/")
        .replace('−', "-")
        .replace('π', "pi")
        .replace('τ', "tau")
        .replace('φ', "phi")
        .replace('√', "sqrt");

    let chars: Vec<char> = cleaned.chars().collect();
    let mut raw = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) {
            let start = i;
            let mut has_dot = false;
            while i < chars.len() {
                if chars[i].is_ascii_digit() {
                    i += 1;
                } else if chars[i] == '.' && !has_dot {
                    has_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            // Scientific notation exponent e.g. 1.5e-3
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let num_str: String = chars[start..i].iter().collect();
            let val = num_str
                .parse::<f64>()
                .map_err(|_| format!("Invalid number: {}", num_str))?;
            raw.push(Token::Num(val));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect::<String>().to_lowercase();
            raw.push(Token::Ident(ident));
            continue;
        }
        let tok = match c {
            '+' => Token::Plus,
            '-' => Token::Minus,
            '*' => Token::Star,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '^' => Token::Caret,
            '!' => Token::Bang,
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            _ => return Err(format!("Unexpected character: '{}'", c)),
        };
        raw.push(tok);
        i += 1;
    }

    if !implicit_mul || raw.len() < 2 {
        return Ok(raw);
    }

    let mut out = Vec::with_capacity(raw.len() + 4);
    for idx in 0..raw.len() {
        let cur = raw[idx].clone();
        if let Some(prev) = out.last() {
            let needs_mul = match (prev, &cur) {
                (Token::Num(_), Token::Ident(_) | Token::LParen) => true,
                (Token::RParen, Token::Num(_) | Token::Ident(_) | Token::LParen) => true,
                (Token::Bang, Token::Num(_) | Token::Ident(_) | Token::LParen) => true,
                (Token::Ident(name), Token::LParen) => !is_known_function(name),
                (Token::Ident(_), Token::Num(_)) => true,
                _ => false,
            };
            if needs_mul {
                out.push(Token::Star);
            }
        }
        out.push(cur);
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        if self.pos < self.tokens.len() {
            let t = self.tokens[self.pos].clone();
            self.pos += 1;
            Some(t)
        } else {
            None
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_add_sub()
    }

    fn parse_add_sub(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_mul_div()?;
        while let Some(tok) = self.peek() {
            let op = match tok {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.next();
            let rhs = self.parse_mul_div()?;
            lhs = Expr::Bin {
                op,
                lhs: std::boxed::Box::new(lhs),
                rhs: std::boxed::Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul_div(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        while let Some(tok) = self.peek() {
            let op = match tok {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Mod,
                _ => break,
            };
            self.next();
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin {
                op,
                lhs: std::boxed::Box::new(lhs),
                rhs: std::boxed::Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    // Standard math precedence: `-x^2` == `-(x^2)` and `2^-3` == `2^(-3)`
    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::Plus) => {
                self.next();
                self.parse_unary()
            }
            Some(Token::Minus) => {
                self.next();
                let inner = self.parse_unary()?;
                Ok(Expr::Neg(std::boxed::Box::new(inner)))
            }
            _ => self.parse_power(),
        }
    }

    fn parse_power(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_postfix()?;
        if matches!(self.peek(), Some(Token::Caret)) {
            self.next();
            let rhs = self.parse_unary()?; // right-associative
            Ok(Expr::Bin {
                op: BinOp::Pow,
                lhs: std::boxed::Box::new(lhs),
                rhs: std::boxed::Box::new(rhs),
            })
        } else {
            Ok(lhs)
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        while matches!(self.peek(), Some(Token::Bang)) {
            self.next();
            expr = Expr::Fact(std::boxed::Box::new(expr));
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Num(n)) => Ok(Expr::Num(n)),
            Some(Token::LParen) => {
                let inner = self.parse_expr()?;
                if !matches!(self.next(), Some(Token::RParen)) {
                    return Err("Missing closing ')'".to_string());
                }
                Ok(inner)
            }
            Some(Token::Ident(name)) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.next();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Token::RParen)) {
                        loop {
                            args.push(self.parse_expr()?);
                            if matches!(self.peek(), Some(Token::Comma)) {
                                self.next();
                            } else {
                                break;
                            }
                        }
                    }
                    if !matches!(self.next(), Some(Token::RParen)) {
                        return Err(format!("Missing ')' after {}(...)", name));
                    }
                    Ok(Expr::Call { name, args })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Some(other) => Err(format!("Unexpected token: {:?}", other)),
            None => Err("Incomplete expression".to_string()),
        }
    }
}

fn parse_expression(input: &str, implicit_mul: bool) -> Result<Expr, String> {
    let tokens = tokenize(input, implicit_mul)?;
    if tokens.is_empty() {
        return Err("Empty expression".to_string());
    }
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr()?;
    if parser.pos < parser.tokens.len() {
        return Err("Unexpected trailing tokens".to_string());
    }
    Ok(expr)
}

fn expr_uses_x(expr: &Expr) -> bool {
    match expr {
        Expr::Num(_) => false,
        Expr::Var(v) => v == "x",
        Expr::Neg(e) | Expr::Fact(e) => expr_uses_x(e),
        Expr::Bin { lhs, rhs, .. } => expr_uses_x(lhs) || expr_uses_x(rhs),
        Expr::Call { args, .. } => args.iter().any(expr_uses_x),
    }
}

/// Lanczos approximation for Gamma(z), enabling continuous `gamma(x)` and `x!`
fn gamma_fn(z: f64) -> f64 {
    const COEFFS: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if z < 0.5 {
        if (z - z.round()).abs() < 1e-12 {
            return f64::NAN;
        }
        PI / ((PI * z).sin() * gamma_fn(1.0 - z))
    } else {
        let z1 = z - 1.0;
        let mut x = COEFFS[0];
        for (i, &c) in COEFFS.iter().enumerate().skip(1) {
            x += c / (z1 + i as f64);
        }
        let t = z1 + 7.5;
        (2.0 * PI).sqrt() * t.powf(z1 + 0.5) * (-t).exp() * x
    }
}

fn factorial_fn(x: f64) -> f64 {
    if x < 0.0 && (x - x.round()).abs() < 1e-12 {
        return f64::NAN;
    }
    if x >= 0.0 && x <= 170.0 && (x - x.round()).abs() < 1e-12 {
        let n = x.round() as u64;
        let mut acc = 1.0_f64;
        for k in 2..=n {
            acc *= k as f64;
        }
        acc
    } else {
        gamma_fn(x + 1.0)
    }
}

fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

struct EvalCtx<'a> {
    angle_mode: AngleMode,
    ans: f64,
    x: Option<f64>,
    vars: &'a BTreeMap<String, f64>,
}

impl<'a> EvalCtx<'a> {
    fn to_rad(&self, a: f64) -> f64 {
        match self.angle_mode {
            AngleMode::Rad => a,
            AngleMode::Deg => a.to_radians(),
            AngleMode::Grad => a * PI / 200.0,
        }
    }

    fn from_rad(&self, r: f64) -> f64 {
        match self.angle_mode {
            AngleMode::Rad => r,
            AngleMode::Deg => r.to_degrees(),
            AngleMode::Grad => r * 200.0 / PI,
        }
    }

    fn eval(&self, expr: &Expr) -> Result<f64, String> {
        match expr {
            Expr::Num(n) => Ok(*n),
            Expr::Var(name) => match name.as_str() {
                "pi" => Ok(PI),
                "tau" => Ok(TAU),
                "e" => Ok(E),
                "phi" => Ok(1.618_033_988_749_895),
                "ans" => Ok(self.ans),
                "x" => self
                    .x
                    .ok_or_else(|| "Variable 'x' is for graphing — click Plot!".to_string()),
                other => self
                    .vars
                    .get(other)
                    .copied()
                    .ok_or_else(|| format!("Unknown variable '{}'", other)),
            },
            Expr::Neg(inner) => Ok(-self.eval(inner)?),
            Expr::Fact(inner) => Ok(factorial_fn(self.eval(inner)?)),
            Expr::Bin { op, lhs, rhs } => {
                let a = self.eval(lhs)?;
                let b = self.eval(rhs)?;
                Ok(match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                    BinOp::Mod => a % b,
                    BinOp::Pow => a.powf(b),
                })
            }
            Expr::Call { name, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let need = |n: usize| -> Result<(), String> {
                    if vals.len() != n {
                        Err(format!("{}() takes {} argument(s)", name, n))
                    } else {
                        Ok(())
                    }
                };
                match name.as_str() {
                    "sin" => { need(1)?; Ok(self.to_rad(vals[0]).sin()) }
                    "cos" => { need(1)?; Ok(self.to_rad(vals[0]).cos()) }
                    "tan" => { need(1)?; Ok(self.to_rad(vals[0]).tan()) }
                    "csc" => { need(1)?; Ok(1.0 / self.to_rad(vals[0]).sin()) }
                    "sec" => { need(1)?; Ok(1.0 / self.to_rad(vals[0]).cos()) }
                    "cot" => { need(1)?; Ok(1.0 / self.to_rad(vals[0]).tan()) }
                    "asin" => { need(1)?; Ok(self.from_rad(vals[0].asin())) }
                    "acos" => { need(1)?; Ok(self.from_rad(vals[0].acos())) }
                    "atan" => { need(1)?; Ok(self.from_rad(vals[0].atan())) }
                    "atan2" => { need(2)?; Ok(self.from_rad(vals[0].atan2(vals[1]))) }
                    "sinh" => { need(1)?; Ok(vals[0].sinh()) }
                    "cosh" => { need(1)?; Ok(vals[0].cosh()) }
                    "tanh" => { need(1)?; Ok(vals[0].tanh()) }
                    "asinh" => { need(1)?; Ok(vals[0].asinh()) }
                    "acosh" => { need(1)?; Ok(vals[0].acosh()) }
                    "atanh" => { need(1)?; Ok(vals[0].atanh()) }
                    "ln" => { need(1)?; Ok(vals[0].ln()) }
                    "log10" => { need(1)?; Ok(vals[0].log10()) }
                    "log2" => { need(1)?; Ok(vals[0].log2()) }
                    "log" => match vals.len() {
                        1 => Ok(vals[0].log10()),
                        2 => Ok(vals[1].log(vals[0])),
                        _ => Err("log() takes 1 arg log(x) or 2 args log(base, x)".to_string()),
                    },
                    "exp" => { need(1)?; Ok(vals[0].exp()) }
                    "sqrt" => { need(1)?; Ok(vals[0].sqrt()) }
                    "cbrt" => { need(1)?; Ok(vals[0].cbrt()) }
                    "pow" => { need(2)?; Ok(vals[0].powf(vals[1])) }
                    "abs" => { need(1)?; Ok(vals[0].abs()) }
                    "floor" => { need(1)?; Ok(vals[0].floor()) }
                    "ceil" => { need(1)?; Ok(vals[0].ceil()) }
                    "round" => { need(1)?; Ok(vals[0].round()) }
                    "sign" => {
                        need(1)?;
                        Ok(if vals[0] > 0.0 {
                            1.0
                        } else if vals[0] < 0.0 {
                            -1.0
                        } else {
                            0.0
                        })
                    }
                    "min" => {
                        if vals.is_empty() {
                            return Err("min() needs at least 1 argument".to_string());
                        }
                        Ok(vals.into_iter().fold(f64::INFINITY, f64::min))
                    }
                    "max" => {
                        if vals.is_empty() {
                            return Err("max() needs at least 1 argument".to_string());
                        }
                        Ok(vals.into_iter().fold(f64::NEG_INFINITY, f64::max))
                    }
                    "clamp" => {
                        need(3)?;
                        Ok(vals[0].clamp(vals[1].min(vals[2]), vals[1].max(vals[2])))
                    }
                    "deg" => { need(1)?; Ok(vals[0].to_degrees()) }
                    "rad" => { need(1)?; Ok(vals[0].to_radians()) }
                    "gamma" => { need(1)?; Ok(gamma_fn(vals[0])) }
                    "ncr" | "comb" => {
                        need(2)?;
                        let n = vals[0];
                        let r = vals[1];
                        Ok(factorial_fn(n) / (factorial_fn(r) * factorial_fn(n - r)))
                    }
                    "npr" | "perm" => {
                        need(2)?;
                        let n = vals[0];
                        let r = vals[1];
                        Ok(factorial_fn(n) / factorial_fn(n - r))
                    }
                    "gcd" => {
                        need(2)?;
                        Ok(gcd_u64(vals[0].abs().round() as u64, vals[1].abs().round() as u64) as f64)
                    }
                    "lcm" => {
                        need(2)?;
                        let a = vals[0].abs().round() as u64;
                        let b = vals[1].abs().round() as u64;
                        let g = gcd_u64(a, b);
                        Ok(if g == 0 { 0.0 } else { ((a / g).saturating_mul(b)) as f64 })
                    }
                    _ => Err(format!("Unknown function '{}()'", name)),
                }
            }
        }
    }
}

fn format_number(val: f64, fmt: NumFormat, prec: usize) -> String {
    if val.is_nan() {
        return "NaN".to_string();
    }
    if val.is_infinite() {
        return if val.is_sign_positive() {
            "∞".to_string()
        } else {
            "-∞".to_string()
        };
    }
    // Snap floating-point noise like sin(pi) = 1.22e-16 cleanly to 0
    let clean = if val.abs() < 1e-13 { 0.0 } else { val };

    match fmt {
        NumFormat::Sci => format!("{:.*e}", prec, clean),
        NumFormat::Eng => {
            if clean == 0.0 {
                return "0".to_string();
            }
            let exp3 = ((clean.abs().log10() / 3.0).floor() as i32) * 3;
            let mant = clean / 10_f64.powi(exp3);
            let s = trim_zeros(&format!("{:.*}", prec, mant));
            if exp3 == 0 {
                s
            } else {
                format!("{}e{:+}", s, exp3)
            }
        }
        NumFormat::Normal => {
            if clean == 0.0 {
                return "0".to_string();
            }
            let abs = clean.abs();
            if abs >= 1e12 || abs < 1e-6 {
                format!("{:.*e}", prec, clean)
            } else {
                trim_zeros(&format!("{:.*}", prec, clean))
            }
        }
    }
}

fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        let t = s.trim_end_matches('0').trim_end_matches('.');
        if t == "-0" {
            "0".to_string()
        } else {
            t.to_string()
        }
    } else {
        s.to_string()
    }
}

fn format_axis_tick(val: f64) -> String {
    if val.abs() < 1e-10 {
        return "0".to_string();
    }
    let abs = val.abs();
    if abs >= 1e5 || abs < 1e-3 {
        format!("{:.1e}", val)
    } else {
        trim_zeros(&format!("{:.3}", val))
    }
}

fn nice_grid_step(span: f64, target_lines: f64) -> f64 {
    let raw = (span.abs() / target_lines.max(2.0)).max(1e-12);
    let pow10 = 10_f64.powf(raw.log10().floor());
    let norm = raw / pow10;
    let nice = if norm < 1.5 {
        1.0
    } else if norm < 3.5 {
        2.0
    } else if norm < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * pow10
}

// =============================================================================
// APP STATE & GRAPH CURVES
// =============================================================================

const CURVE_PALETTE: [(f64, f64, f64); 4] = [
    (0.32, 0.88, 0.98), // Electric Cyan
    (0.96, 0.45, 0.78), // Neon Pink
    (0.98, 0.72, 0.25), // Warm Amber
    (0.45, 0.94, 0.62), // Mint Green
];

const CURVE_DOT_GLYPHS: [&str; 4] = ["●", "●", "●", "●"];

#[derive(Clone)]
struct PlottedCurve {
    raw: String,
    ast: Option<Expr>,
    error: Option<String>,
}

struct AppState {
    settings: SciCalcSettings,
    ans: f64,
    memory: f64,
    last_result_str: String,
    history: Vec<(String, String)>,
    curves: Vec<PlottedCurve>,
    // Graph viewport
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    drag_origin: Option<(f64, f64, f64, f64)>,
    hover_px: Option<(f64, f64)>,
}

impl AppState {
    fn new() -> Self {
        let settings = load_settings();
        let mut st = Self {
            ans: 0.0,
            memory: 0.0,
            last_result_str: "0".to_string(),
            history: Vec::new(),
            curves: Vec::new(),
            x_min: -10.0,
            x_max: 10.0,
            y_min: -6.5,
            y_max: 6.5,
            drag_origin: None,
            hover_px: None,
            settings,
        };
        st.rebuild_curve_asts();
        st
    }

    fn rebuild_curve_asts(&mut self) {
        let imp = self.settings.implicit_mul;
        self.curves = self
            .settings
            .saved_curves
            .iter()
            .take(4)
            .map(|raw| {
                let cleaned = raw
                    .trim()
                    .strip_prefix("y=")
                    .or_else(|| raw.trim().strip_prefix("y ="))
                    .or_else(|| raw.trim().strip_prefix("f(x)="))
                    .or_else(|| raw.trim().strip_prefix("f(x) ="))
                    .unwrap_or(raw.trim());
                match parse_expression(cleaned, imp) {
                    Ok(ast) => PlottedCurve {
                        raw: cleaned.to_string(),
                        ast: Some(ast),
                        error: None,
                    },
                    Err(e) => PlottedCurve {
                        raw: cleaned.to_string(),
                        ast: None,
                        error: Some(e),
                    },
                }
            })
            .collect();
    }

    fn add_or_focus_curve(&mut self, expr_str: &str) {
        let cleaned = expr_str
            .trim()
            .strip_prefix("y=")
            .or_else(|| expr_str.trim().strip_prefix("y ="))
            .or_else(|| expr_str.trim().strip_prefix("f(x)="))
            .or_else(|| expr_str.trim().strip_prefix("f(x) ="))
            .unwrap_or(expr_str.trim())
            .to_string();
        if cleaned.is_empty() {
            return;
        }
        if !self.settings.saved_curves.iter().any(|c| c == &cleaned) {
            if self.settings.saved_curves.len() >= 4 {
                self.settings.saved_curves.remove(0);
            }
            self.settings.saved_curves.push(cleaned);
            save_settings(&self.settings);
            self.rebuild_curve_asts();
        }
    }

    fn eval_text(&self, raw: &str) -> Result<(Option<String>, f64, bool), String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(String::new());
        }

        // Check variable assignment: `name = expr`
        if let Some((lhs, rhs)) = trimmed.split_once('=') {
            let var_name = lhs.trim().to_lowercase();
            let valid_var = !var_name.is_empty()
                && var_name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_alphabetic() || c == '_')
                    .unwrap_or(false)
                && var_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');
            let reserved = matches!(var_name.as_str(), "pi" | "tau" | "e" | "phi" | "ans" | "x" | "y")
                || is_known_function(&var_name);

            if valid_var && !reserved {
                let ast = parse_expression(rhs.trim(), self.settings.implicit_mul)?;
                let ctx = EvalCtx {
                    angle_mode: self.settings.angle_mode,
                    ans: self.ans,
                    x: None,
                    vars: &self.settings.variables,
                };
                let val = ctx.eval(&ast)?;
                return Ok((Some(var_name), val, false));
            }
        }

        let ast = parse_expression(trimmed, self.settings.implicit_mul)?;
        if expr_uses_x(&ast) {
            return Ok((None, f64::NAN, true));
        }
        let ctx = EvalCtx {
            angle_mode: self.settings.angle_mode,
            ans: self.ans,
            x: None,
            vars: &self.settings.variables,
        };
        let val = ctx.eval(&ast)?;
        Ok((None, val, false))
    }

    fn auto_fit_y(&mut self) {
        let ctx = EvalCtx {
            angle_mode: self.settings.angle_mode,
            ans: self.ans,
            x: None,
            vars: &self.settings.variables,
        };
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let steps = 240;
        let span = self.x_max - self.x_min;
        for c in &self.curves {
            let Some(ref ast) = c.ast else { continue };
            for i in 0..=steps {
                let x = self.x_min + (i as f64 / steps as f64) * span;
                let ec = EvalCtx {
                    angle_mode: ctx.angle_mode,
                    ans: ctx.ans,
                    x: Some(x),
                    vars: ctx.vars,
                };
                if let Ok(y) = ec.eval(ast) {
                    if y.is_finite() && y.abs() < 1e6 {
                        lo = lo.min(y);
                        hi = hi.max(y);
                    }
                }
            }
        }
        if lo.is_finite() && hi.is_finite() {
            let pad = ((hi - lo) * 0.15).max(1.0);
            self.y_min = lo - pad;
            self.y_max = hi + pad;
        }
    }
}

fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

fn clear_box(b: &Box) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

// =============================================================================
// CAIRO GRAPH RENDERER
// =============================================================================

fn draw_graph(cr: &cairo::Context, w: i32, h: i32, st: &AppState) {
    let wf = w.max(1) as f64;
    let hf = h.max(1) as f64;

    // Smoked-glass oscilloscope backdrop (looks crisp on both Dark and Light themes)
    cr.rectangle(0.0, 0.0, wf, hf);
    cr.set_source_rgba(0.05, 0.05, 0.09, 0.72);
    let _ = cr.fill();

    let x_span = (st.x_max - st.x_min).max(1e-9);
    let y_span = (st.y_max - st.y_min).max(1e-9);
    let to_px = |x: f64| (x - st.x_min) / x_span * wf;
    let to_py = |y: f64| hf - (y - st.y_min) / y_span * hf;

    // 1. Grid lines & numeric tick labels
    let x_step = nice_grid_step(x_span, 7.0);
    let y_step = nice_grid_step(y_span, 6.0);

    if st.settings.show_grid {
        cr.set_line_width(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);

        let mut gx = (st.x_min / x_step).floor() * x_step;
        let mut guard = 0;
        while gx <= st.x_max && guard < 100 {
            let px = to_px(gx);
            cr.move_to(px, 0.0);
            cr.line_to(px, hf);
            gx += x_step;
            guard += 1;
        }

        let mut gy = (st.y_min / y_step).floor() * y_step;
        guard = 0;
        while gy <= st.y_max && guard < 100 {
            let py = to_py(gy);
            cr.move_to(0.0, py);
            cr.line_to(wf, py);
            gy += y_step;
            guard += 1;
        }
        let _ = cr.stroke();
    }

    // 2. X & Y Main Axes
    let origin_x = to_px(0.0);
    let origin_y = to_py(0.0);
    cr.set_line_width(1.4);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.36);
    if origin_y >= 0.0 && origin_y <= hf {
        cr.move_to(0.0, origin_y);
        cr.line_to(wf, origin_y);
    }
    if origin_x >= 0.0 && origin_x <= wf {
        cr.move_to(origin_x, 0.0);
        cr.line_to(origin_x, hf);
    }
    let _ = cr.stroke();

    // 3. Axis Tick Labels
    if st.settings.show_labels {
        cr.select_font_face("monospace", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        cr.set_font_size(9.5);
        cr.set_source_rgba(0.92, 0.92, 0.98, 0.65);

        let label_y = origin_y.clamp(12.0, hf - 4.0);
        let mut gx = (st.x_min / x_step).floor() * x_step;
        let mut guard = 0;
        while gx <= st.x_max && guard < 80 {
            if gx.abs() > x_step * 0.25 {
                let px = to_px(gx);
                if px > 14.0 && px < wf - 22.0 {
                    cr.move_to(px + 2.0, label_y - 3.0);
                    let _ = cr.show_text(&format_axis_tick(gx));
                }
            }
            gx += x_step;
            guard += 1;
        }

        let label_x = origin_x.clamp(4.0, wf - 34.0);
        let mut gy = (st.y_min / y_step).floor() * y_step;
        guard = 0;
        while gy <= st.y_max && guard < 80 {
            if gy.abs() > y_step * 0.25 {
                let py = to_py(gy);
                if py > 12.0 && py < hf - 6.0 {
                    cr.move_to(label_x + 4.0, py - 2.0);
                    let _ = cr.show_text(&format_axis_tick(gy));
                }
            }
            gy += y_step;
            guard += 1;
        }
    }

    // 4. Plot Each Active Curve
    let samples = st.settings.graph_samples.clamp(120, 2000);
    let line_w = st.settings.curve_width.clamp(1.0, 5.0);

    for (idx, curve) in st.curves.iter().enumerate() {
        let Some(ref ast) = curve.ast else { continue };
        let (r, g, b) = CURVE_PALETTE[idx % CURVE_PALETTE.len()];
        cr.set_source_rgba(r, g, b, 0.95);
        cr.set_line_width(line_w);

        let mut pen_down = false;
        let mut prev_py = 0.0_f64;

        for s in 0..=samples {
            let x = st.x_min + (s as f64 / samples as f64) * x_span;
            let ctx = EvalCtx {
                angle_mode: st.settings.angle_mode,
                ans: st.ans,
                x: Some(x),
                vars: &st.settings.variables,
            };
            match ctx.eval(ast) {
                Ok(y) if y.is_finite() => {
                    let px = to_px(x);
                    let py = to_py(y);
                    // Break path across vertical asymptotes (e.g. tan(x), 1/x)
                    let huge_jump = pen_down && (py - prev_py).abs() > hf * 0.85;
                    let offscreen = py < -hf * 4.0 || py > hf * 5.0;
                    if offscreen {
                        pen_down = false;
                    } else if !pen_down || huge_jump {
                        cr.move_to(px, py);
                        pen_down = true;
                    } else {
                        cr.line_to(px, py);
                    }
                    prev_py = py;
                }
                _ => {
                    pen_down = false;
                }
            }
        }
        let _ = cr.stroke();
    }

    // 5. Interactive Hover Crosshair + Snap Dot on f₁(x)
    if st.settings.show_trace {
        if let Some((hx, hy)) = st.hover_px {
            cr.set_line_width(1.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
            cr.move_to(hx, 0.0);
            cr.line_to(hx, hf);
            cr.move_to(0.0, hy);
            cr.line_to(wf, hy);
            let _ = cr.stroke();

            // Snap glowing trace dot onto first valid curve at hovered x
            let math_x = st.x_min + (hx / wf) * x_span;
            for (idx, curve) in st.curves.iter().enumerate() {
                if let Some(ref ast) = curve.ast {
                    let ctx = EvalCtx {
                        angle_mode: st.settings.angle_mode,
                        ans: st.ans,
                        x: Some(math_x),
                        vars: &st.settings.variables,
                    };
                    if let Ok(math_y) = ctx.eval(ast) {
                        if math_y.is_finite() {
                            let py = to_py(math_y);
                            if py >= 0.0 && py <= hf {
                                let (r, g, b) = CURVE_PALETTE[idx % CURVE_PALETTE.len()];
                                cr.set_source_rgba(r, g, b, 0.35);
                                cr.arc(hx, py, 7.5, 0.0, TAU);
                                let _ = cr.fill();
                                cr.set_source_rgba(r, g, b, 1.0);
                                cr.arc(hx, py, 3.8, 0.0, TAU);
                                let _ = cr.fill();
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
}

// =============================================================================
// UI BUILDER
// =============================================================================

pub fn build(_ctx: &crate::module_api::ModuleContext) -> crate::module_api::ModuleHandle {
    let state = Rc::new(RefCell::new(AppState::new()));

    // --- LEFT PANE (Scientific Calculator + History) ---
    let root = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
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

    let title_lbl = Label::builder()
        .label("SCIENTIFIC CALC")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();

    let graph_toggle_btn = Button::builder()
        .label("Graph ▸")
        .css_classes(vec!["plan-close-btn".to_string()])
        .tooltip_text("Toggle 400px Graphing & Settings panel")
        .build();

    hdr_row.append(&title_lbl);
    hdr_row.append(&graph_toggle_btn);
    root.append(&hdr_row);

    // Quick Mode Pill Strip (Angle + Format + Memory badge)
    let mode_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let angle_quick_btn = Button::builder()
        .label(match state.borrow().settings.angle_mode {
            AngleMode::Rad => "RAD",
            AngleMode::Deg => "DEG",
            AngleMode::Grad => "GRAD",
        })
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .tooltip_text("Click to cycle Radians / Degrees / Gradians")
        .build();

    let fmt_quick_btn = Button::builder()
        .label(match state.borrow().settings.num_format {
            NumFormat::Normal => "NORM",
            NumFormat::Sci => "SCI",
            NumFormat::Eng => "ENG",
        })
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Click to cycle Normal / Scientific / Engineering notation")
        .build();

    let mem_badge = Label::builder()
        .label("M: 0")
        .halign(Align::End)
        .hexpand(true)
        .css_classes(vec!["fav-score-badge".to_string()])
        .build();

    mode_row.append(&angle_quick_btn);
    mode_row.append(&fmt_quick_btn);
    mode_row.append(&mem_badge);
    root.append(&mode_row);

    // Display Card (Expression Entry + Live Result + Copy)
    let disp_card = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let expr_entry = Entry::builder()
        .placeholder_text("sin(pi/4)^2, 5!, a = 3, x^2 - 4...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();

    let res_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();

    let result_lbl = Label::builder()
        .label("0")
        .halign(Align::End)
        .hexpand(true)
        .width_chars(1)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .css_classes(vec!["plan-h1".to_string()])
        .build();

    let copy_res_btn = Button::builder()
        .label("Copy")
        .valign(Align::Center)
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Copy result to clipboard")
        .build();

    res_row.append(&result_lbl);
    res_row.append(&copy_res_btn);

    disp_card.append(&expr_entry);
    disp_card.append(&res_row);
    root.append(&disp_card);

    // Scientific Keypad Grid (8 rows x 5 cols)
    let keypad = Grid::builder()
        .row_spacing(4)
        .column_spacing(4)
        .row_homogeneous(true)
        .column_homogeneous(true)
        .hexpand(true)
        .build();

    let keys: [[(&str, &str); 5]; 8] = [
        [("sin", "sin("), ("cos", "cos("), ("tan", "tan("), ("asin", "asin("), ("acos", "acos(")],
        [("atan", "atan("), ("ln", "ln("), ("log", "log("), ("√", "sqrt("), ("abs", "abs(")],
        [("π", "pi"), ("e", "e"), ("^", "^"), ("x!", "!"), ("ans", "ans")],
        [("MC", "__MC"), ("MR", "__MR"), ("M+", "__M+"), ("(", "("), (")", ")")],
        [("7", "7"), ("8", "8"), ("9", "9"), ("÷", "/"), ("⌫", "__BS")],
        [("4", "4"), ("5", "5"), ("6", "6"), ("×", "*"), ("C", "__CLR")],
        [("1", "1"), ("2", "2"), ("3", "3"), ("−", "-"), ("x", "x")],
        [("0", "0"), (".", "."), ("%", "%"), ("+", "+"), ("=", "__EQ")],
    ];

    // Plot Shortcut Button
    let plot_cur_btn = Button::builder()
        .label("Plot Expression in Graph →")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .tooltip_text("Send expression containing 'x' to the interactive grapher")
        .build();

    // History Header & List
    let hist_hdr_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(2)
        .build();
    hist_hdr_row.append(
        &Label::builder()
            .label("HISTORY")
            .halign(Align::Start)
            .hexpand(true)
            .css_classes(vec!["fav-group-header".to_string()])
            .build(),
    );
    let clear_hist_btn = Button::builder()
        .label("Clear")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    hist_hdr_row.append(&clear_hist_btn);

    let hist_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();
    let hist_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .build();
    hist_scroll.set_child(Some(&hist_box));

    // --- RIGHT 400PX SLIDE-OUT DETAIL PANE (Grapher + Settings) ---
    let detail_view = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["plan-detail-pane".to_string()])
        .width_request(400)
        .vexpand(true)
        .visible(false)
        .build();

    let d_top = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .margin_top(12)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(6)
        .build();

    let d_tab_graph = Button::builder()
        .label("Grapher")
        .css_classes(vec!["plan-fmt-btn".to_string(), "active".to_string()])
        .hexpand(true)
        .build();
    let d_tab_sett = Button::builder()
        .label("Settings")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .hexpand(true)
        .build();
    let d_close_btn = Button::builder()
        .label("×")
        .css_classes(vec!["plan-line-del-btn".to_string()])
        .build();
    d_top.append(&d_tab_graph);
    d_top.append(&d_tab_sett);
    d_top.append(&d_close_btn);
    detail_view.append(&d_top);

    let d_stack = Stack::new();
    d_stack.set_hexpand(true);
    d_stack.set_vexpand(true);
    detail_view.append(&d_stack);

    // DETAIL TAB 1: GRAPHER
    let graph_page = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let canvas_frame = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["sys-card".to_string()])
        .build();

    let canvas = DrawingArea::builder()
        .content_width(366)
        .content_height(260)
        .hexpand(true)
        .build();
    canvas_frame.append(&canvas);
    graph_page.append(&canvas_frame);

    let trace_lbl = Label::builder()
        .label("Drag to pan • Scroll to zoom • Hover to trace")
        .halign(Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .css_classes(vec!["plan-subnote".to_string()])
        .build();
    graph_page.append(&trace_lbl);

    // Viewport Buttons
    let vp_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(5)
        .homogeneous(true)
        .build();
    let btn_zin = Button::builder()
        .label("Zoom +")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let btn_zout = Button::builder()
        .label("Zoom −")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let btn_reset = Button::builder()
        .label("10×10")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let btn_trig = Button::builder()
        .label("±2π")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    let btn_autoy = Button::builder()
        .label("Auto-Y")
        .css_classes(vec!["plan-fmt-btn".to_string()])
        .build();
    vp_row.append(&btn_zin);
    vp_row.append(&btn_zout);
    vp_row.append(&btn_reset);
    vp_row.append(&btn_trig);
    vp_row.append(&btn_autoy);
    graph_page.append(&vp_row);

    // Add Function Input Row
    let fn_add_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(6)
        .build();
    let fn_entry = Entry::builder()
        .placeholder_text("y = sin(x), x^3 - 3x, 1/x...")
        .css_classes(vec!["fav-search".to_string()])
        .hexpand(true)
        .build();
    let fn_add_btn = Button::builder()
        .label("+ Plot")
        .css_classes(vec!["primary-btn".to_string()])
        .build();
    fn_add_row.append(&fn_entry);
    fn_add_row.append(&fn_add_btn);
    graph_page.append(&fn_add_row);

    // Quick Preset Curves Row
    let preset_row = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .homogeneous(true)
        .build();
    let presets: &[(&str, &str)] = &[
        ("sin(x)", "sin(x)"),
        ("x²−4", "x^2 - 4"),
        ("tan(x)", "tan(x)"),
        ("sinc", "sin(x)/x"),
        ("bell", "exp(-x^2/2)"),
    ];
    graph_page.append(&preset_row);

    let curves_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();
    let curves_list_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(5)
        .build();
    curves_scroll.set_child(Some(&curves_list_box));
    graph_page.append(&curves_scroll);

    // DETAIL TAB 2: SETTINGS
    let sett_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .build();
    let sett_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(10)
        .margin_start(14)
        .margin_end(14)
        .margin_bottom(14)
        .build();
    sett_scroll.set_child(Some(&sett_box));

    d_stack.add_named(&graph_page, Some("graph"));
    d_stack.add_named(&sett_scroll, Some("settings"));

    // =========================================================================
    // REACTIVE UI HELPERS
    // =========================================================================

    let sync_mode_pills: Rc<dyn Fn()> = {
        let st_m = state.clone();
        let ab = angle_quick_btn.clone();
        let fb = fmt_quick_btn.clone();
        let mb = mem_badge.clone();
        Rc::new(move || {
            let st = st_m.borrow();
            ab.set_label(match st.settings.angle_mode {
                AngleMode::Rad => "RAD",
                AngleMode::Deg => "DEG",
                AngleMode::Grad => "GRAD",
            });
            fb.set_label(match st.settings.num_format {
                NumFormat::Normal => "NORM",
                NumFormat::Sci => "SCI",
                NumFormat::Eng => "ENG",
            });
            let m_txt = format!(
                "M: {}",
                format_number(st.memory, st.settings.num_format, st.settings.precision.min(4))
            );
            mb.set_label(&m_txt);
        })
    };

    let eval_live_preview: Rc<dyn Fn()> = {
        let st_e = state.clone();
        let ent = expr_entry.clone();
        let res = result_lbl.clone();
        Rc::new(move || {
            let text = ent.text().to_string();
            if text.trim().is_empty() {
                res.set_label("0");
                return;
            }
            let st = st_e.borrow();
            match st.eval_text(&text) {
                Ok((Some(var), val, _)) => {
                    let s = format!(
                        "{} = {}",
                        var,
                        format_number(val, st.settings.num_format, st.settings.precision)
                    );
                    res.set_label(&s);
                }
                Ok((None, _, true)) => {
                    res.set_label("f(x) — Press Enter to Plot");
                }
                Ok((None, val, false)) => {
                    let s = format_number(val, st.settings.num_format, st.settings.precision);
                    res.set_label(&s);
                }
                Err(_) => {
                    // Keep subtle indication while still typing
                }
            }
        })
    };

    let render_history: Rc<dyn Fn()> = {
        let st_h = state.clone();
        let hb = hist_box.clone();
        let ent = expr_entry.clone();
        Rc::new(move || {
            clear_box(&hb);
            let hist = st_h.borrow().history.clone();
            if hist.is_empty() {
                hb.append(
                    &Label::builder()
                        .label("No calculations yet.")
                        .halign(Align::Start)
                        .css_classes(vec!["plan-subnote".to_string()])
                        .build(),
                );
                return;
            }
            for (expr, res) in hist.into_iter().rev().take(25) {
                let btn = Button::builder()
                    .css_classes(vec!["fav-item-btn".to_string()])
                    .hexpand(true)
                    .tooltip_text("Click to load expression")
                    .build();
                let r = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(6)
                    .build();
                r.append(
                    &Label::builder()
                        .label(&expr)
                        .halign(Align::Start)
                        .hexpand(true)
                        .width_chars(1)
                        .ellipsize(gtk4::pango::EllipsizeMode::End)
                        .css_classes(vec!["plan-subnote".to_string()])
                        .build(),
                );
                r.append(
                    &Label::builder()
                        .label(&format!("= {}", res))
                        .halign(Align::End)
                        .css_classes(vec!["fav-score-badge".to_string()])
                        .build(),
                );
                btn.set_child(Some(&r));
                let e_c = ent.clone();
                let expr_load = expr.clone();
                btn.connect_clicked(move |_| {
                    e_c.set_text(&expr_load);
                    e_c.set_position(-1);
                    e_c.grab_focus();
                });
                hb.append(&btn);
            }
        })
    };

    let render_curves_list: Rc<dyn Fn()> = {
        let st_c = state.clone();
        let clb = curves_list_box.clone();
        let canv = canvas.clone();
        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_slot.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            clear_box(&clb);
            let curves = st_c.borrow().curves.clone();
            if curves.is_empty() {
                clb.append(
                    &Label::builder()
                        .label("No functions plotted. Enter y = f(x) above!")
                        .halign(Align::Start)
                        .css_classes(vec!["plan-subnote".to_string()])
                        .build(),
                );
                return;
            }
            for (idx, curve) in curves.iter().enumerate() {
                let row = Box::builder()
                    .orientation(Orientation::Horizontal)
                    .spacing(6)
                    .css_classes(vec!["plan-check-row".to_string()])
                    .build();

                let (r, g, b) = CURVE_PALETTE[idx % CURVE_PALETTE.len()];
                let hex = format!(
                    "#{:02x}{:02x}{:02x}",
                    (r * 255.0) as u8,
                    (g * 255.0) as u8,
                    (b * 255.0) as u8
                );
                let dot = Label::builder().valign(Align::Center).build();
                dot.set_markup(&format!(
                    "<span foreground=\"{}\">{}</span>",
                    hex,
                    CURVE_DOT_GLYPHS[idx % 4]
                ));

                let label_text = if let Some(ref err) = curve.error {
                    format!("y{} = {}  ({})", idx + 1, curve.raw, err)
                } else {
                    format!("y{} = {}", idx + 1, curve.raw)
                };
                let lbl = Label::builder()
                    .label(&label_text)
                    .halign(Align::Start)
                    .hexpand(true)
                    .width_chars(1)
                    .ellipsize(gtk4::pango::EllipsizeMode::End)
                    .css_classes(vec!["plan-body".to_string()])
                    .build();

                let del_btn = Button::builder()
                    .label("×")
                    .valign(Align::Center)
                    .css_classes(vec!["plan-line-del-btn".to_string()])
                    .build();

                let st_d = st_c.clone();
                let cv_d = canv.clone();
                let re_d = self_weak.clone();
                del_btn.connect_clicked(move |_| {
                    {
                        let mut st = st_d.borrow_mut();
                        if idx < st.settings.saved_curves.len() {
                            st.settings.saved_curves.remove(idx);
                            save_settings(&st.settings);
                            st.rebuild_curve_asts();
                        }
                    }
                    cv_d.queue_draw();
                    if let Some(ref f) = *re_d.borrow() {
                        f();
                    }
                });

                row.append(&dot);
                row.append(&lbl);
                row.append(&del_btn);
                clb.append(&row);
            }
        });

        *self_slot.borrow_mut() = Some(cb.clone());
        cb
    };

    let render_settings: Rc<dyn Fn()> = {
        let st_s = state.clone();
        let sb = sett_box.clone();
        let canv = canvas.clone();
        let sync_m = sync_mode_pills.clone();
        let eval_l = eval_live_preview.clone();
        let rc_list = render_curves_list.clone();
        let self_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let self_weak = self_slot.clone();

        let cb: Rc<dyn Fn()> = Rc::new(move || {
            clear_box(&sb);
            let snapshot = {
                let st = st_s.borrow();
                (st.settings.clone(), st.x_min, st.x_max, st.y_min, st.y_max)
            };
            let (cfg, x_min, x_max, y_min, y_max) = snapshot;

            let rebuild_all = {
                let cv = canv.clone();
                let sm = sync_m.clone();
                let el = eval_l.clone();
                let rl = rc_list.clone();
                let sw = self_weak.clone();
                Rc::new(move || {
                    sm();
                    el();
                    rl();
                    cv.queue_draw();
                    if let Some(ref f) = *sw.borrow() {
                        f();
                    }
                })
            };

            // 1. Calculation Settings Card
            sb.append(
                &Label::builder()
                    .label("CALCULATION")
                    .halign(Align::Start)
                    .css_classes(vec!["fav-group-header".to_string()])
                    .build(),
            );
            let calc_card = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(8)
                .css_classes(vec!["sys-card".to_string()])
                .build();

            // Angle mode row
            calc_card.append(
                &Label::builder()
                    .label("Angle Unit")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let ang_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            for &(m, lbl) in &[
                (AngleMode::Rad, "Radians"),
                (AngleMode::Deg, "Degrees"),
                (AngleMode::Grad, "Gradians"),
            ] {
                let b = Button::builder()
                    .label(lbl)
                    .css_classes(if cfg.angle_mode == m {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.angle_mode = m;
                        save_settings(&st.settings);
                    }
                    ra();
                });
                ang_row.append(&b);
            }
            calc_card.append(&ang_row);

            // Number Format row
            calc_card.append(
                &Label::builder()
                    .label("Number Notation")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let fmt_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            for &(m, lbl) in &[
                (NumFormat::Normal, "Normal"),
                (NumFormat::Sci, "Scientific"),
                (NumFormat::Eng, "Engineering"),
            ] {
                let b = Button::builder()
                    .label(lbl)
                    .css_classes(if cfg.num_format == m {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.num_format = m;
                        save_settings(&st.settings);
                    }
                    ra();
                });
                fmt_row.append(&b);
            }
            calc_card.append(&fmt_row);

            // Precision row
            calc_card.append(
                &Label::builder()
                    .label("Decimal Precision")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let prec_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(4)
                .homogeneous(true)
                .build();
            for p in [2usize, 4, 6, 8, 10, 12] {
                let b = Button::builder()
                    .label(&p.to_string())
                    .css_classes(if cfg.precision == p {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.precision = p;
                        save_settings(&st.settings);
                    }
                    ra();
                });
                prec_row.append(&b);
            }
            calc_card.append(&prec_row);

            // Implicit Multiplication toggle
            let imp_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(8)
                .build();
            imp_row.append(
                &Label::builder()
                    .label("Implicit Multiply (2pi, 3x, 2sin(x))")
                    .halign(Align::Start)
                    .hexpand(true)
                    .css_classes(vec!["plan-body".to_string()])
                    .build(),
            );
            let imp_btn = Button::builder()
                .label(if cfg.implicit_mul { "ON" } else { "OFF" })
                .css_classes(if cfg.implicit_mul {
                    vec!["plan-fmt-btn".to_string(), "active".to_string()]
                } else {
                    vec!["plan-fmt-btn".to_string()]
                })
                .build();
            {
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                imp_btn.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.implicit_mul = !st.settings.implicit_mul;
                        save_settings(&st.settings);
                        st.rebuild_curve_asts();
                    }
                    ra();
                });
            }
            imp_row.append(&imp_btn);
            calc_card.append(&imp_row);
            sb.append(&calc_card);

            // 2. Graphing Settings Card
            sb.append(
                &Label::builder()
                    .label("GRAPHING")
                    .halign(Align::Start)
                    .css_classes(vec!["fav-group-header".to_string()])
                    .margin_top(4)
                    .build(),
            );
            let graph_card = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(8)
                .css_classes(vec!["sys-card".to_string()])
                .build();

            let tog_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();

            let add_gtog = |lbl: &str, active: bool, which: &'static str| {
                let b = Button::builder()
                    .label(lbl)
                    .css_classes(if active {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        match which {
                            "grid" => st.settings.show_grid = !st.settings.show_grid,
                            "labels" => st.settings.show_labels = !st.settings.show_labels,
                            "trace" => st.settings.show_trace = !st.settings.show_trace,
                            _ => {}
                        }
                        save_settings(&st.settings);
                    }
                    ra();
                });
                tog_row.append(&b);
            };
            add_gtog("Grid", cfg.show_grid, "grid");
            add_gtog("Labels", cfg.show_labels, "labels");
            add_gtog("Crosshair", cfg.show_trace, "trace");
            graph_card.append(&tog_row);

            // Curve Width
            graph_card.append(
                &Label::builder()
                    .label("Curve Thickness")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let w_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            for &(wval, lbl) in &[(1.4, "Thin"), (2.2, "Medium"), (3.4, "Thick")] {
                let active = (cfg.curve_width - wval).abs() < 0.25;
                let b = Button::builder()
                    .label(lbl)
                    .css_classes(if active {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.curve_width = wval;
                        save_settings(&st.settings);
                    }
                    ra();
                });
                w_row.append(&b);
            }
            graph_card.append(&w_row);

            // Sample Resolution
            graph_card.append(
                &Label::builder()
                    .label("Plot Resolution (samples / curve)")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let res_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            for &(sval, lbl) in &[(300usize, "Fast 300"), (600, "High 600"), (1200, "Ultra 1200")] {
                let b = Button::builder()
                    .label(lbl)
                    .css_classes(if cfg.graph_samples == sval {
                        vec!["plan-fmt-btn".to_string(), "active".to_string()]
                    } else {
                        vec!["plan-fmt-btn".to_string()]
                    })
                    .build();
                let st_c = st_s.clone();
                let ra = rebuild_all.clone();
                b.connect_clicked(move |_| {
                    {
                        let mut st = st_c.borrow_mut();
                        st.settings.graph_samples = sval;
                        save_settings(&st.settings);
                    }
                    ra();
                });
                res_row.append(&b);
            }
            graph_card.append(&res_row);

            // Manual Viewport Bounds
            graph_card.append(
                &Label::builder()
                    .label("Viewport Bounds (X min/max, Y min/max)")
                    .halign(Align::Start)
                    .css_classes(vec!["plan-subnote".to_string()])
                    .build(),
            );
            let bx_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            let e_xmin = Entry::builder()
                .text(&format_axis_tick(x_min))
                .placeholder_text("X min")
                .css_classes(vec!["fav-search".to_string()])
                .build();
            let e_xmax = Entry::builder()
                .text(&format_axis_tick(x_max))
                .placeholder_text("X max")
                .css_classes(vec!["fav-search".to_string()])
                .build();
            bx_row.append(&e_xmin);
            bx_row.append(&e_xmax);

            let by_row = Box::builder()
                .orientation(Orientation::Horizontal)
                .spacing(5)
                .homogeneous(true)
                .build();
            let e_ymin = Entry::builder()
                .text(&format_axis_tick(y_min))
                .placeholder_text("Y min")
                .css_classes(vec!["fav-search".to_string()])
                .build();
            let e_ymax = Entry::builder()
                .text(&format_axis_tick(y_max))
                .placeholder_text("Y max")
                .css_classes(vec!["fav-search".to_string()])
                .build();
            by_row.append(&e_ymin);
            by_row.append(&e_ymax);

            let apply_bounds_btn = Button::builder()
                .label("Apply Bounds")
                .css_classes(vec!["primary-btn".to_string()])
                .build();
            {
                let st_c = st_s.clone();
                let cv = canv.clone();
                let (x0, x1, y0, y1) = (e_xmin.clone(), e_xmax.clone(), e_ymin.clone(), e_ymax.clone());
                apply_bounds_btn.connect_clicked(move |_| {
                    let mut st = st_c.borrow_mut();
                    let imp = st.settings.implicit_mul;
                    let parse_b = |txt: &str| -> Option<f64> {
                        let ast = parse_expression(txt, imp).ok()?;
                        let ctx = EvalCtx {
                            angle_mode: st.settings.angle_mode,
                            ans: st.ans,
                            x: None,
                            vars: &st.settings.variables,
                        };
                        ctx.eval(&ast).ok().filter(|v| v.is_finite())
                    };
                    let (nx0, nx1, ny0, ny1) = (
                        parse_b(&x0.text()),
                        parse_b(&x1.text()),
                        parse_b(&y0.text()),
                        parse_b(&y1.text()),
                    );
                    if let (Some(a), Some(b)) = (nx0, nx1) {
                        if b > a {
                            st.x_min = a;
                            st.x_max = b;
                        }
                    }
                    if let (Some(a), Some(b)) = (ny0, ny1) {
                        if b > a {
                            st.y_min = a;
                            st.y_max = b;
                        }
                    }
                    drop(st);
                    cv.queue_draw();
                });
            }
            graph_card.append(&bx_row);
            graph_card.append(&by_row);
            graph_card.append(&apply_bounds_btn);
            sb.append(&graph_card);

            // 3. Saved Variables Card
            sb.append(
                &Label::builder()
                    .label("USER VARIABLES (type e.g. a = 2.5)")
                    .halign(Align::Start)
                    .css_classes(vec!["fav-group-header".to_string()])
                    .margin_top(4)
                    .build(),
            );
            let var_card = Box::builder()
                .orientation(Orientation::Vertical)
                .spacing(5)
                .css_classes(vec!["sys-card".to_string()])
                .build();
            if cfg.variables.is_empty() {
                var_card.append(
                    &Label::builder()
                        .label("No variables defined yet.\nAssign one in the calculator (e.g. k = 3) to use it in both math & graphs!")
                        .halign(Align::Start)
                        .wrap(true)
                        .xalign(0.0)
                        .css_classes(vec!["plan-subnote".to_string()])
                        .build(),
                );
            } else {
                for (k, v) in &cfg.variables {
                    let vr = Box::builder()
                        .orientation(Orientation::Horizontal)
                        .spacing(6)
                        .build();
                    vr.append(
                        &Label::builder()
                            .label(&format!(
                                "{} = {}",
                                k,
                                format_number(*v, cfg.num_format, cfg.precision)
                            ))
                            .halign(Align::Start)
                            .hexpand(true)
                            .css_classes(vec!["plan-body".to_string()])
                            .build(),
                    );
                    let del = Button::builder()
                        .label("×")
                        .css_classes(vec!["plan-line-del-btn".to_string()])
                        .build();
                    let k_del = k.clone();
                    let st_c = st_s.clone();
                    let ra = rebuild_all.clone();
                    del.connect_clicked(move |_| {
                        {
                            let mut st = st_c.borrow_mut();
                            st.settings.variables.remove(&k_del);
                            save_settings(&st.settings);
                        }
                        ra();
                    });
                    vr.append(&del);
                    var_card.append(&vr);
                }
            }
            sb.append(&var_card);
        });

        *self_slot.borrow_mut() = Some(cb.clone());
        cb
    };

    // =========================================================================
    // COMMIT EVALUATION & PLOT ACTIONS
    // =========================================================================

    let commit_eval: Rc<dyn Fn()> = {
        let st_c = state.clone();
        let ent = expr_entry.clone();
        let res = result_lbl.clone();
        let dv = detail_view.clone();
        let ds = d_stack.clone();
        let tg = d_tab_graph.clone();
        let ts = d_tab_sett.clone();
        let gbtn = graph_toggle_btn.clone();
        let canv = canvas.clone();
        let rh = render_history.clone();
        let rc = render_curves_list.clone();
        let rs = render_settings.clone();
        let sm = sync_mode_pills.clone();

        Rc::new(move || {
            let text = ent.text().trim().to_string();
            if text.is_empty() {
                return;
            }
            let eval_res = st_c.borrow().eval_text(&text);
            match eval_res {
                Ok((Some(var), val, _)) => {
                    let formatted = {
                        let mut st = st_c.borrow_mut();
                        st.settings.variables.insert(var.clone(), val);
                        save_settings(&st.settings);
                        st.ans = val;
                        let f = format_number(val, st.settings.num_format, st.settings.precision);
                        st.last_result_str = f.clone();
                        st.history.push((text.clone(), format!("{} = {}", var, f)));
                        f
                    };
                    let disp = format!("{} = {}", var, formatted);
                    res.set_label(&disp);
                    rh();
                    rs();
                    canv.queue_draw();
                }
                Ok((None, _, true)) => {
                    // Expression contains `x` -> open Grapher and plot it!
                    {
                        let mut st = st_c.borrow_mut();
                        st.add_or_focus_curve(&text);
                    }
                    dv.set_visible(true);
                    gbtn.set_label("Graph ▾");
                    ds.set_visible_child_name("graph");
                    tg.add_css_class("active");
                    ts.remove_css_class("active");
                    res.set_label("Plotted →");
                    rc();
                    canv.queue_draw();
                }
                Ok((None, val, false)) => {
                    let formatted = {
                        let mut st = st_c.borrow_mut();
                        st.ans = val;
                        let f = format_number(val, st.settings.num_format, st.settings.precision);
                        st.last_result_str = f.clone();
                        st.history.push((text.clone(), f.clone()));
                        f
                    };
                    res.set_label(&formatted);
                    sm();
                    rh();
                }
                Err(err) => {
                    res.set_label(&err);
                }
            }
        })
    };

    // Populate Keypad Buttons
    for (r, row_keys) in keys.iter().enumerate() {
        for (c, &(label, token)) in row_keys.iter().enumerate() {
            let css = if token == "__EQ" {
                vec!["primary-btn".to_string()]
            } else if token.starts_with("__") || r < 3 {
                vec!["plan-fmt-btn".to_string()]
            } else {
                vec!["fav-item-btn".to_string()]
            };

            let btn = Button::builder()
                .label(label)
                .css_classes(css)
                .hexpand(true)
                .build();

            let ent = expr_entry.clone();
            let res = result_lbl.clone();
            let st_k = state.clone();
            let eval_fn = commit_eval.clone();
            let sm = sync_mode_pills.clone();
            let tok = token.to_string();

            btn.connect_clicked(move |_| match tok.as_str() {
                "__EQ" => eval_fn(),
                "__CLR" => {
                    ent.set_text("");
                    res.set_label("0");
                    ent.grab_focus();
                }
                "__BS" => {
                    let mut s = ent.text().to_string();
                    s.pop();
                    ent.set_text(&s);
                    ent.set_position(-1);
                    ent.grab_focus();
                }
                "__MC" => {
                    st_k.borrow_mut().memory = 0.0;
                    sm();
                }
                "__MR" => {
                    let mem_str = {
                        let st = st_k.borrow();
                        format_number(st.memory, st.settings.num_format, st.settings.precision)
                    };
                    let mut s = ent.text().to_string();
                    s.push_str(&mem_str);
                    ent.set_text(&s);
                    ent.set_position(-1);
                    ent.grab_focus();
                }
                "__M+" => {
                    let txt = ent.text().to_string();
                    let add_val = {
                        let st = st_k.borrow();
                        match st.eval_text(&txt) {
                            Ok((_, v, false)) if v.is_finite() => v,
                            _ => st.ans,
                        }
                    };
                    st_k.borrow_mut().memory += add_val;
                    sm();
                }
                insert => {
                    let mut s = ent.text().to_string();
                    s.push_str(insert);
                    ent.set_text(&s);
                    ent.set_position(-1);
                    ent.grab_focus();
                }
            });

            keypad.attach(&btn, c as i32, r as i32, 1, 1);
        }
    }

    root.append(&keypad);
    root.append(&plot_cur_btn);
    root.append(&hist_hdr_row);
    root.append(&hist_scroll);

    // =========================================================================
    // WIRE ALL SIGNALS
    // =========================================================================

    {
        let ev = eval_live_preview.clone();
        expr_entry.connect_changed(move |_| ev());
    }
    {
        let ce = commit_eval.clone();
        expr_entry.connect_activate(move |_| ce());
    }

    // Copy result
    {
        let st_cp = state.clone();
        let res_cp = result_lbl.clone();
        copy_res_btn.connect_clicked(move |b| {
            let txt = res_cp.text().to_string();
            let copy_val = if txt.contains('=') {
                st_cp.borrow().last_result_str.clone()
            } else {
                txt
            };
            copy_to_clipboard(&copy_val);
            b.set_label("Copied!");
            let br = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1000), move || {
                br.set_label("Copy");
            });
        });
    }

    // Quick Angle & Format Cycling
    {
        let st_a = state.clone();
        let sm = sync_mode_pills.clone();
        let ev = eval_live_preview.clone();
        let rs = render_settings.clone();
        let cv = canvas.clone();
        angle_quick_btn.connect_clicked(move |_| {
            {
                let mut st = st_a.borrow_mut();
                st.settings.angle_mode = match st.settings.angle_mode {
                    AngleMode::Rad => AngleMode::Deg,
                    AngleMode::Deg => AngleMode::Grad,
                    AngleMode::Grad => AngleMode::Rad,
                };
                save_settings(&st.settings);
            }
            sm();
            ev();
            rs();
            cv.queue_draw();
        });
    }
    {
        let st_f = state.clone();
        let sm = sync_mode_pills.clone();
        let ev = eval_live_preview.clone();
        let rs = render_settings.clone();
        fmt_quick_btn.connect_clicked(move |_| {
            {
                let mut st = st_f.borrow_mut();
                st.settings.num_format = match st.settings.num_format {
                    NumFormat::Normal => NumFormat::Sci,
                    NumFormat::Sci => NumFormat::Eng,
                    NumFormat::Eng => NumFormat::Normal,
                };
                save_settings(&st.settings);
            }
            sm();
            ev();
            rs();
        });
    }

    // Clear history
    {
        let st_ch = state.clone();
        let rh = render_history.clone();
        clear_hist_btn.connect_clicked(move |_| {
            st_ch.borrow_mut().history.clear();
            rh();
        });
    }

    // Plot current expression button
    {
        let ent = expr_entry.clone();
        let st_p = state.clone();
        let dv = detail_view.clone();
        let ds = d_stack.clone();
        let tg = d_tab_graph.clone();
        let ts = d_tab_sett.clone();
        let gbtn = graph_toggle_btn.clone();
        let rc = render_curves_list.clone();
        let cv = canvas.clone();
        plot_cur_btn.connect_clicked(move |_| {
            let txt = ent.text().trim().to_string();
            if !txt.is_empty() {
                st_p.borrow_mut().add_or_focus_curve(&txt);
                rc();
            }
            dv.set_visible(true);
            gbtn.set_label("Graph ▾");
            ds.set_visible_child_name("graph");
            tg.add_css_class("active");
            ts.remove_css_class("active");
            cv.queue_draw();
        });
    }

    // Toggle slide-out detail pane
    {
        let dv = detail_view.clone();
        let rs = render_settings.clone();
        let rc = render_curves_list.clone();
        let cv = canvas.clone();
        graph_toggle_btn.connect_clicked(move |b| {
            let next = !dv.is_visible();
            dv.set_visible(next);
            b.set_label(if next { "Graph ▾" } else { "Graph ▸" });
            if next {
                rc();
                rs();
                cv.queue_draw();
            }
        });
    }
    {
        let dv = detail_view.clone();
        let gbtn = graph_toggle_btn.clone();
        d_close_btn.connect_clicked(move |_| {
            dv.set_visible(false);
            gbtn.set_label("Graph ▸");
        });
    }
    {
        let ds = d_stack.clone();
        let tg = d_tab_graph.clone();
        let ts = d_tab_sett.clone();
        let cv = canvas.clone();
        d_tab_graph.connect_clicked(move |_| {
            ds.set_visible_child_name("graph");
            tg.add_css_class("active");
            ts.remove_css_class("active");
            cv.queue_draw();
        });
    }
    {
        let ds = d_stack.clone();
        let tg = d_tab_graph.clone();
        let ts = d_tab_sett.clone();
        let rs = render_settings.clone();
        d_tab_sett.connect_clicked(move |_| {
            ds.set_visible_child_name("settings");
            ts.add_css_class("active");
            tg.remove_css_class("active");
            rs();
        });
    }

    // Plot new curve from Grapher input
    let add_curve_action: Rc<dyn Fn()> = {
        let fe = fn_entry.clone();
        let st_a = state.clone();
        let rc = render_curves_list.clone();
        let cv = canvas.clone();
        Rc::new(move || {
            let txt = fe.text().trim().to_string();
            if txt.is_empty() {
                return;
            }
            st_a.borrow_mut().add_or_focus_curve(&txt);
            fe.set_text("");
            rc();
            cv.queue_draw();
        })
    };
    {
        let ac = add_curve_action.clone();
        fn_add_btn.connect_clicked(move |_| ac());
    }
    {
        let ac = add_curve_action.clone();
        fn_entry.connect_activate(move |_| ac());
    }

    // Preset curve chips
    for &(lbl, expr_s) in presets {
        let b = Button::builder()
            .label(lbl)
            .css_classes(vec!["plan-fmt-btn".to_string()])
            .build();
        let st_p = state.clone();
        let rc = render_curves_list.clone();
        let cv = canvas.clone();
        let expr_owned = expr_s.to_string();
        b.connect_clicked(move |_| {
            st_p.borrow_mut().add_or_focus_curve(&expr_owned);
            rc();
            cv.queue_draw();
        });
        preset_row.append(&b);
    }

    // Viewport controls (Zoom In/Out, Reset, Trig, Auto-Y)
    {
        let st_z = state.clone();
        let cv = canvas.clone();
        btn_zin.connect_clicked(move |_| {
            {
                let mut st = st_z.borrow_mut();
                let cx = (st.x_min + st.x_max) * 0.5;
                let cy = (st.y_min + st.y_max) * 0.5;
                let hx = (st.x_max - st.x_min) * 0.35;
                let hy = (st.y_max - st.y_min) * 0.35;
                st.x_min = cx - hx;
                st.x_max = cx + hx;
                st.y_min = cy - hy;
                st.y_max = cy + hy;
            }
            cv.queue_draw();
        });
    }
    {
        let st_z = state.clone();
        let cv = canvas.clone();
        btn_zout.connect_clicked(move |_| {
            {
                let mut st = st_z.borrow_mut();
                let cx = (st.x_min + st.x_max) * 0.5;
                let cy = (st.y_min + st.y_max) * 0.5;
                let hx = (st.x_max - st.x_min) * 0.72;
                let hy = (st.y_max - st.y_min) * 0.72;
                st.x_min = cx - hx;
                st.x_max = cx + hx;
                st.y_min = cy - hy;
                st.y_max = cy + hy;
            }
            cv.queue_draw();
        });
    }
    {
        let st_r = state.clone();
        let cv = canvas.clone();
        btn_reset.connect_clicked(move |_| {
            {
                let mut st = st_r.borrow_mut();
                st.x_min = -10.0;
                st.x_max = 10.0;
                st.y_min = -6.5;
                st.y_max = 6.5;
            }
            cv.queue_draw();
        });
    }
    {
        let st_t = state.clone();
        let cv = canvas.clone();
        btn_trig.connect_clicked(move |_| {
            {
                let mut st = st_t.borrow_mut();
                st.x_min = -TAU;
                st.x_max = TAU;
                st.y_min = -2.5;
                st.y_max = 2.5;
            }
            cv.queue_draw();
        });
    }
    {
        let st_a = state.clone();
        let cv = canvas.clone();
        btn_autoy.connect_clicked(move |_| {
            st_a.borrow_mut().auto_fit_y();
            cv.queue_draw();
        });
    }

    // =========================================================================
    // INTERACTIVE CAIRO CANVAS (Draw + Drag Pan + Scroll Zoom + Hover Trace)
    // =========================================================================

    {
        let st_d = state.clone();
        canvas.set_draw_func(move |_, cr, w, h| {
            let st = st_d.borrow();
            draw_graph(cr, w, h, &st);
        });
    }

    // Drag to Pan
    let drag = GestureDrag::new();
    {
        let st_b = state.clone();
        drag.connect_drag_begin(move |_, _, _| {
            let mut st = st_b.borrow_mut();
            st.drag_origin = Some((st.x_min, st.x_max, st.y_min, st.y_max));
        });
    }
    {
        let st_u = state.clone();
        let cv = canvas.clone();
        drag.connect_drag_update(move |_, ox, oy| {
            let w = cv.width().max(1) as f64;
            let h = cv.height().max(1) as f64;
            {
                let mut st = st_u.borrow_mut();
                if let Some((x0, x1, y0, y1)) = st.drag_origin {
                    let dx = -(ox / w) * (x1 - x0);
                    let dy = (oy / h) * (y1 - y0);
                    st.x_min = x0 + dx;
                    st.x_max = x1 + dx;
                    st.y_min = y0 + dy;
                    st.y_max = y1 + dy;
                }
            }
            cv.queue_draw();
        });
    }
    {
        let st_e = state.clone();
        drag.connect_drag_end(move |_, _, _| {
            st_e.borrow_mut().drag_origin = None;
        });
    }
    canvas.add_controller(drag);

    // Scroll Wheel to Zoom (around cursor if hovering)
    let scroll_ctrl = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    {
        let st_s = state.clone();
        let cv = canvas.clone();
        scroll_ctrl.connect_scroll(move |_, _, dy| {
            let factor = if dy > 0.0 { 1.18 } else { 0.85 };
            let w = cv.width().max(1) as f64;
            let h = cv.height().max(1) as f64;
            {
                let mut st = st_s.borrow_mut();
                let (rx, ry) = if let Some((hx, hy)) = st.hover_px {
                    ((hx / w).clamp(0.0, 1.0), (1.0 - hy / h).clamp(0.0, 1.0))
                } else {
                    (0.5, 0.5)
                };
                let cx = st.x_min + rx * (st.x_max - st.x_min);
                let cy = st.y_min + ry * (st.y_max - st.y_min);
                let new_xs = ((st.x_max - st.x_min) * factor).clamp(1e-6, 1e8);
                let new_ys = ((st.y_max - st.y_min) * factor).clamp(1e-6, 1e8);
                st.x_min = cx - rx * new_xs;
                st.x_max = st.x_min + new_xs;
                st.y_min = cy - ry * new_ys;
                st.y_max = st.y_min + new_ys;
            }
            cv.queue_draw();
            glib::Propagation::Stop
        });
    }
    canvas.add_controller(scroll_ctrl);

    // Hover Crosshair + Curve Trace Readout
    let motion = EventControllerMotion::new();
    {
        let st_m = state.clone();
        let cv = canvas.clone();
        let tl = trace_lbl.clone();
        motion.connect_motion(move |_, hx, hy| {
            let w = cv.width().max(1) as f64;
            let h = cv.height().max(1) as f64;
            let readout = {
                let mut st = st_m.borrow_mut();
                st.hover_px = Some((hx, hy));
                let mx = st.x_min + (hx / w) * (st.x_max - st.x_min);
                let my = st.y_max - (hy / h) * (st.y_max - st.y_min);
                let mut text = format!("x = {:.4}  |  y = {:.4}", mx, my);
                if let Some(first) = st.curves.first() {
                    if let Some(ref ast) = first.ast {
                        let ctx = EvalCtx {
                            angle_mode: st.settings.angle_mode,
                            ans: st.ans,
                            x: Some(mx),
                            vars: &st.settings.variables,
                        };
                        if let Ok(fy) = ctx.eval(ast) {
                            if fy.is_finite() {
                                text = format!(
                                    "x = {:.4}  |  f₁(x) = {}",
                                    mx,
                                    format_number(fy, st.settings.num_format, 4)
                                );
                            }
                        }
                    }
                }
                text
            };
            tl.set_label(&readout);
            cv.queue_draw();
        });
    }
    {
        let st_l = state.clone();
        let cv = canvas.clone();
        let tl = trace_lbl.clone();
        motion.connect_leave(move |_| {
            st_l.borrow_mut().hover_px = None;
            tl.set_label("Drag to pan • Scroll to zoom • Hover to trace");
            cv.queue_draw();
        });
    }
    canvas.add_controller(motion);

    // Initial Renders
    sync_mode_pills();
    render_history();
    render_curves_list();
    render_settings();

    let focus_entry = expr_entry.clone();
    let gbtn_hide = graph_toggle_btn.clone();
    crate::module_api::ModuleHandle {
        widget: root.upcast::<gtk4::Widget>(),
        detail_pane: Some(detail_view.upcast::<gtk4::Widget>()),
        on_show: Some(Rc::new(move || {
            focus_entry.grab_focus();
        })),
        on_hide: Some(Rc::new(move || {
            gbtn_hide.set_label("Graph ▸");
        })),
    }
}
