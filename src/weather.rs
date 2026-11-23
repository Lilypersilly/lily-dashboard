use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Align, Box, Button, GestureClick, Label, Orientation, PolicyType, ScrolledWindow, SearchEntry, Stack,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WeatherConfig {
    pub city_name: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Deserialize, Debug)]
struct GeoResponse {
    results: Option<Vec<GeoResult>>,
}

#[derive(Deserialize, Clone, Debug)]
struct GeoResult {
    name: String,
    latitude: f64,
    longitude: f64,
    country: Option<String>,
    admin1: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct CurrentData {
    temperature_2m: f64,
    weather_code: u8,
    precipitation: f64,
    wind_speed_10m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
}

#[derive(Deserialize, Debug, Clone)]
struct DailyData {
    time: Vec<String>,
    weather_code: Vec<u8>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_probability_max: Vec<u8>,
    wind_speed_10m_max: Vec<f64>,
    uv_index_max: Vec<f64>,
    sunrise: Vec<String>,
    sunset: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct WeatherResponse {
    current: CurrentData,
    daily: DailyData,
}

enum WeatherMsg {
    Forecast(Option<WeatherResponse>),
    SearchResults(Option<Vec<GeoResult>>),
}

fn config_path() -> PathBuf {
    let mut path = glib::user_config_dir();
    path.push("lily-dashboard");
    let _ = fs::create_dir_all(&path);
    path.push("weather.json");
    path
}

fn load_config() -> Option<WeatherConfig> {
    let data = fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_config(cfg: &WeatherConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), json);
    }
}

// Maps WMO weather codes to Emojis and descriptions
fn wmo_to_text(code: u8) -> (&'static str, &'static str) {
    match code {
        0 => ("☀️", "Clear"),
        1 => ("🌤️", "Mostly Clear"),
        2 => ("⛅", "Partly Cloudy"),
        3 => ("☁️", "Overcast"),
        45 | 48 => ("🌫️", "Fog"),
        51 | 53 | 55 => ("🌧️", "Drizzle"),
        56 | 57 => ("🌧️", "Frz Drizzle"),
        61 | 63 | 65 => ("🌧️", "Rain"),
        66 | 67 => ("🌧️", "Frz Rain"),
        71 | 73 | 75 => ("❄️", "Snow"),
        77 => ("❄️", "Snow Grains"),
        80 | 81 | 82 => ("🌦️", "Showers"),
        85 | 86 => ("❄️", "Snow Showers"),
        95 | 96 | 99 => ("⛈️", "Thunderstorm"),
        _ => ("❓", "Unknown"),
    }
}

fn format_date(date: &str) -> String {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() == 3 {
        format!("{}/{}", parts[1], parts[2])
    } else {
        date.to_string()
    }
}

// Helper to pull "07:45" out of "2026-11-21T07:45"
fn extract_time(datetime: &str) -> &str {
    datetime.split('T').nth(1).unwrap_or(datetime)
}

// Injects the custom selected/hover states into the GTK stylesheet
fn init_weather_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(
        ".weather-row { transition: all 0.15s ease; cursor: pointer; }
         .weather-row:hover { background-color: rgba(255, 255, 255, 0.05); }
         .weather-row.selected {
             border-width: 2px !important;
             background-color: rgba(255, 255, 255, 0.12) !important;
         }"
    );
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn build_weather_ui(main_stack: &Stack) -> (Box, Button) {
    // Initialize our custom CSS for the selected row highlights
    init_weather_css();

    let config = Rc::new(RefCell::new(load_config()));
    let latest_weather_data: Rc<RefCell<Option<WeatherResponse>>> = Rc::new(RefCell::new(None));
    let (tx, rx) = mpsc::channel::<WeatherMsg>();

    // ==========================================
    // 1. SIDEBAR WIDGET
    // ==========================================
    let sidebar_lbl = Label::builder()
        .label("Setup\nWeather")
        .justify(gtk4::Justification::Center)
        .css_classes(vec!["session-restore-lbl".to_string()])
        .build();

    let sidebar_btn = Button::builder()
        .child(&sidebar_lbl)
        .css_classes(vec!["session-restore-btn".to_string()])
        .tooltip_text("Open Weather Forecast")
        .build();

    let stack_clone = main_stack.clone();
    sidebar_btn.connect_clicked(move |_| {
        stack_clone.set_visible_child_name("weather");
    });

    // ==========================================
    // 2. FULL FORECAST APP TAB
    // ==========================================
    let app_box = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .hexpand(true)
        .vexpand(true)
        .build();

    let inner_stack = Stack::new();
    inner_stack.set_hhomogeneous(false);
    inner_stack.set_hexpand(true);
    inner_stack.set_vexpand(true);
    app_box.append(&inner_stack);

    // --- VIEW FORECAST PAGE ---
    let forecast_page = Box::builder().orientation(Orientation::Vertical).spacing(8).build();
    
    let top_bar = Box::builder().orientation(Orientation::Horizontal).spacing(6).build();
    let city_title_lbl = Label::builder()
        .label("WEATHER")
        .halign(Align::Start)
        .hexpand(true)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let change_city_btn = Button::builder()
        .label("Change City")
        .css_classes(vec!["plan-close-btn".to_string()])
        .build();
    top_bar.append(&city_title_lbl);
    top_bar.append(&change_city_btn);
    forecast_page.append(&top_bar);

    // Dynamic Top "Details" Box
    let details_container = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .margin_top(4)
        .margin_bottom(10)
        .build();

    let selected_date_lbl = Label::builder().css_classes(vec!["plan-step".to_string()]).halign(Align::Center).build();
    
    let main_row = Box::builder().orientation(Orientation::Horizontal).spacing(12).halign(Align::Center).build();
    let big_emoji_lbl = Label::builder().css_classes(vec!["plan-h1".to_string()]).build();
    let big_temp_lbl = Label::builder().css_classes(vec!["plan-h1".to_string()]).build();
    main_row.append(&big_emoji_lbl);
    main_row.append(&big_temp_lbl);

    let desc_lbl = Label::builder().css_classes(vec!["fav-score-badge".to_string()]).halign(Align::Center).build();

    let stats_grid = Box::builder().orientation(Orientation::Horizontal).spacing(32).halign(Align::Center).margin_top(8).build();
    let col1 = Box::builder().orientation(Orientation::Vertical).spacing(4).halign(Align::Start).build();
    let col2 = Box::builder().orientation(Orientation::Vertical).spacing(4).halign(Align::Start).build();
    
    let wind_lbl = Label::builder().css_classes(vec!["plan-body".to_string()]).build();
    let sec_lbl = Label::builder().css_classes(vec!["plan-body".to_string()]).build();
    let sun_lbl = Label::builder().css_classes(vec!["plan-body".to_string()]).build();
    let rain_lbl = Label::builder().css_classes(vec!["plan-body".to_string()]).build();
    
    col1.append(&wind_lbl);
    col1.append(&sec_lbl);
    col2.append(&sun_lbl);
    col2.append(&rain_lbl);
    stats_grid.append(&col1);
    stats_grid.append(&col2);

    details_container.append(&selected_date_lbl);
    details_container.append(&main_row);
    details_container.append(&desc_lbl);
    details_container.append(&stats_grid);
    forecast_page.append(&details_container);

    let list_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vscrollbar_policy(PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();
    let daily_list_box = Box::builder().orientation(Orientation::Vertical).spacing(6).build();
    list_scroll.set_child(Some(&daily_list_box));
    forecast_page.append(&list_scroll);

    // --- SEARCH / SETUP PAGE ---
    let search_page = Box::builder().orientation(Orientation::Vertical).spacing(8).build();
    
    let search_hdr = Label::builder()
        .label("SEARCH FOR YOUR CITY")
        .halign(Align::Start)
        .css_classes(vec!["fav-group-header".to_string()])
        .build();
    let search_entry = SearchEntry::builder()
        .placeholder_text("e.g. Stockholm, Tokyo...")
        .css_classes(vec!["fav-search".to_string()])
        .build();
    
    let results_box = Box::builder().orientation(Orientation::Vertical).spacing(6).margin_top(6).build();
    
    search_page.append(&search_hdr);
    search_page.append(&search_entry);
    search_page.append(&results_box);

    inner_stack.add_named(&forecast_page, Some("forecast"));
    inner_stack.add_named(&search_page, Some("search"));

    if config.borrow().is_none() {
        inner_stack.set_visible_child_name("search");
    } else {
        inner_stack.set_visible_child_name("forecast");
    }

    // ==========================================
    // 3. LOGIC & BACKGROUND FETCHING
    // ==========================================
    
    // Switch to search page
    let is_search = inner_stack.clone();
    change_city_btn.connect_clicked(move |_| {
        is_search.set_visible_child_name("search");
    });

    // Handle City Search
    let tx_search = tx.clone();
    search_entry.connect_activate(move |entry| {
        let query = entry.text().trim().replace(' ', "+");
        if query.is_empty() { return; }
        let tx_w = tx_search.clone();
        std::thread::spawn(move || {
            let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=5&language=en&format=json", query);
            let out = Command::new("curl").args(["-sL", "--max-time", "5", &url]).output().ok();
            let mut res = None;
            if let Some(output) = out {
                if let Ok(json) = serde_json::from_slice::<GeoResponse>(&output.stdout) {
                    res = json.results;
                }
            }
            let _ = tx_w.send(WeatherMsg::SearchResults(res));
        });
    });

    // Background Forecast Fetcher function
    let fetch_forecast = move |cfg: WeatherConfig, tx_c: mpsc::Sender<WeatherMsg>| {
        std::thread::spawn(move || {
            let url = format!("https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max,wind_speed_10m_max&timezone=auto&forecast_days=14", cfg.lat, cfg.lon);
            let out = Command::new("curl").args(["-sL", "--max-time", "5", &url]).output().ok();
            let mut res = None;
            if let Some(output) = out {
                if let Ok(json) = serde_json::from_slice::<WeatherResponse>(&output.stdout) {
                    res = Some(json);
                }
            }
            let _ = tx_c.send(WeatherMsg::Forecast(res));
        });
    };

    // Auto-fetch on startup if configured
    if let Some(cfg) = config.borrow().as_ref() {
        city_title_lbl.set_label(&format!("WEATHER: {}", cfg.city_name.to_uppercase()));
        fetch_forecast(cfg.clone(), tx.clone());
    }

    // Periodic Background Refresher (Every 30 mins)
    let tx_bg = tx.clone();
    let config_bg = config.clone();
    glib::timeout_add_seconds_local(1800, move || {
        if let Some(cfg) = config_bg.borrow().as_ref() {
            let cfg_clone = cfg.clone(); 
            let tx_c = tx_bg.clone();
            std::thread::spawn(move || {
                let url = format!("https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,uv_index_max,precipitation_probability_max,wind_speed_10m_max&timezone=auto&forecast_days=14", cfg_clone.lat, cfg_clone.lon);
                if let Ok(out) = Command::new("curl").args(["-sL", "--max-time", "5", &url]).output() {
                    if let Ok(json) = serde_json::from_slice::<WeatherResponse>(&out.stdout) {
                        let _ = tx_c.send(WeatherMsg::Forecast(Some(json)));
                    }
                }
            });
        }
        glib::ControlFlow::Continue
    });

    // ==========================================
    // 4. DYNAMIC RENDER CLOSURE
    // ==========================================
    let render_details = {
        let sd_lbl = selected_date_lbl.clone();
        let be_lbl = big_emoji_lbl.clone();
        let bt_lbl = big_temp_lbl.clone();
        let d_lbl = desc_lbl.clone();
        let w_lbl = wind_lbl.clone();
        let sec_lbl = sec_lbl.clone();
        let sun_lbl = sun_lbl.clone();
        let rain_lbl = rain_lbl.clone();

        Rc::new(move |data: &WeatherResponse, idx: usize| {
            let (emoji, desc) = wmo_to_text(data.daily.weather_code[idx]);
            be_lbl.set_label(emoji);
            d_lbl.set_label(desc);

            let s_rise = extract_time(&data.daily.sunrise[idx]);
            let s_set = extract_time(&data.daily.sunset[idx]);
            sun_lbl.set_label(&format!("☀️ {} - {}", s_rise, s_set));

            let rain_prob = data.daily.precipitation_probability_max[idx];
            rain_lbl.set_label(&format!("🌧️ {}% Rain Chance", rain_prob));

            if idx == 0 {
                // Today's Live Current Data
                sd_lbl.set_label("Today's Current Weather");
                let (c_emoji, c_desc) = wmo_to_text(data.current.weather_code);
                be_lbl.set_label(c_emoji);
                d_lbl.set_label(c_desc);
                
                bt_lbl.set_label(&format!("{:.1}°C", data.current.temperature_2m));
                w_lbl.set_label(&format!("💨 {:.1} km/h", data.current.wind_speed_10m));
                sec_lbl.set_label(&format!("💧 {:.0}% (Feels {:.0}°C)", data.current.relative_humidity_2m, data.current.apparent_temperature));
            } else {
                // Future Days Data
                if idx == 1 {
                    sd_lbl.set_label("Tomorrow's Forecast");
                } else {
                    sd_lbl.set_label(&format!("Forecast for {}", format_date(&data.daily.time[idx])));
                }
                let high = data.daily.temperature_2m_max[idx];
                let low = data.daily.temperature_2m_min[idx];
                
                bt_lbl.set_label(&format!("{:.0}° / {:.0}°C", high, low));
                w_lbl.set_label(&format!("💨 Max {:.1} km/h", data.daily.wind_speed_10m_max[idx]));
                sec_lbl.set_label(&format!("🔆 Max UV Index: {:.1}", data.daily.uv_index_max[idx]));
            }
        })
    };

    // ==========================================
    // 5. EVENT LOOP / UI UPDATER
    // ==========================================
    let nav = inner_stack.clone();
    let cfg_rc = config.clone();
    let tx_for_select = tx.clone();
    let data_rc = latest_weather_data.clone();
    let day_rows: Rc<RefCell<Vec<Box>>> = Rc::new(RefCell::new(Vec::new()));

    glib::timeout_add_local(Duration::from_millis(150), move || {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WeatherMsg::SearchResults(Some(results)) => {
                    while let Some(child) = results_box.first_child() {
                        results_box.remove(&child);
                    }
                    if results.is_empty() {
                        let lbl = Label::builder().label("No cities found.").css_classes(vec!["fav-score-badge".to_string()]).build();
                        results_box.append(&lbl);
                    } else {
                        for res in results {
                            let btn = Button::builder().css_classes(vec!["fav-item-btn".to_string()]).build();
                            let label_str = format!("{}, {}", res.name, res.admin1.unwrap_or(res.country.unwrap_or_default()));
                            let lbl = Label::builder().label(&label_str).halign(Align::Start).build();
                            btn.set_child(Some(&lbl));
                            
                            let cfg_c = cfg_rc.clone();
                            let nav_c = nav.clone();
                            let tx_c = tx_for_select.clone();
                            let title_lbl = city_title_lbl.clone();
                            
                            btn.connect_clicked(move |_| {
                                let new_cfg = WeatherConfig {
                                    city_name: res.name.clone(),
                                    lat: res.latitude,
                                    lon: res.longitude,
                                };
                                save_config(&new_cfg);
                                *cfg_c.borrow_mut() = Some(new_cfg.clone());
                                title_lbl.set_label(&format!("WEATHER: {}", new_cfg.city_name.to_uppercase()));
                                nav_c.set_visible_child_name("forecast");
                                fetch_forecast(new_cfg, tx_c.clone());
                            });
                            results_box.append(&btn);
                        }
                    }
                }
                WeatherMsg::SearchResults(None) => {
                    let lbl = Label::builder().label("Search failed (Network error?)").css_classes(vec!["fav-score-badge".to_string()]).build();
                    while let Some(child) = results_box.first_child() { results_box.remove(&child); }
                    results_box.append(&lbl);
                }
                WeatherMsg::Forecast(Some(data)) => {
                    // Save for the click handlers
                    *data_rc.borrow_mut() = Some(data.clone());

                    // Default to showing Today (idx 0) on load
                    render_details(&data, 0);

                    // Update Sidebar Widget
                    let (emoji, desc) = wmo_to_text(data.current.weather_code);
                    let line2 = if data.current.precipitation > 0.0 {
                        format!("{:.1}mm Rain", data.current.precipitation)
                    } else {
                        desc.to_string()
                    };
                    sidebar_lbl.set_label(&format!("{:.0}°C {}\n{}", data.current.temperature_2m, emoji, line2));

                    while let Some(child) = daily_list_box.first_child() {
                        daily_list_box.remove(&child);
                    }
                    day_rows.borrow_mut().clear();

                    for i in 0..data.daily.time.len() {
                        let date_str = if i == 0 { "Today".to_string() } 
                                       else if i == 1 { "Tmrw".to_string() } 
                                       else { format_date(&data.daily.time[i]) };
                        
                        let (d_emoji, d_desc) = wmo_to_text(data.daily.weather_code[i]);
                        let high = data.daily.temperature_2m_max[i];
                        let low = data.daily.temperature_2m_min[i];
                        let rain_prob = data.daily.precipitation_probability_max[i];

                        let mut classes = vec!["plan-check-row".to_string(), "weather-row".to_string()];
                        if i == 0 {
                            classes.push("selected".to_string()); // Default selection
                        }

                        let row = Box::builder()
                            .orientation(Orientation::Horizontal)
                            .spacing(6)
                            .css_classes(classes)
                            .tooltip_text("Click to view details")
                            .build();

                        let date_lbl = Label::builder().label(&date_str).width_chars(5).halign(Align::Start).css_classes(vec!["plan-step".to_string()]).build();
                        let icon_lbl = Label::builder().label(d_emoji).width_chars(2).build();
                        let desc_lbl = Label::builder().label(d_desc).halign(Align::Start).hexpand(true).ellipsize(gtk4::pango::EllipsizeMode::End).build();
                        let temp_lbl = Label::builder().label(&format!("{:.0}° / {:.0}°", high, low)).css_classes(vec!["fav-score-badge".to_string()]).build();
                        
                        row.append(&date_lbl);
                        row.append(&icon_lbl);
                        row.append(&desc_lbl);

                        if rain_prob > 0 {
                            let rain_lbl = Label::builder().label(&format!("{}%", rain_prob)).css_classes(vec!["plan-subnote".to_string()]).margin_end(6).build();
                            row.append(&rain_lbl);
                        }
                        
                        row.append(&temp_lbl);

                        day_rows.borrow_mut().push(row.clone());

                        // Make the row clickable to update the top details view and visual selection
                        let click_controller = GestureClick::builder().button(1).build();
                        let d_clone = data_rc.clone();
                        let rd_clone = render_details.clone();
                        let rows_clone = day_rows.clone();
                        let idx = i;
                        
                        click_controller.connect_pressed(move |_, _, _, _| {
                            if let Some(d) = d_clone.borrow().as_ref() {
                                rd_clone(d, idx);
                            }
                            // Swap the visual 'selected' state!
                            for (j, r) in rows_clone.borrow().iter().enumerate() {
                                if j == idx {
                                    r.add_css_class("selected");
                                } else {
                                    r.remove_css_class("selected");
                                }
                            }
                        });
                        
                        row.add_controller(click_controller);
                        daily_list_box.append(&row);
                    }
                }
                WeatherMsg::Forecast(None) => {
                    sidebar_lbl.set_label("Weather\nError");
                    big_emoji_lbl.set_label("❌");
                    big_temp_lbl.set_label("Offline");
                    desc_lbl.set_label("Check your internet connection.");
                }
            }
        }
        glib::ControlFlow::Continue
    });

    (app_box, sidebar_btn)
}