mod active_watch;
mod config;
mod hotkey;
mod sessions;

use config::Config;
use gtk::prelude::*;
use sessions::{focus_session, Session, TitleCache, Usage};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Minimal info needed to jump to a session, in displayed order.
#[derive(Clone)]
struct NavTarget {
    host_pid: i32,
    project: String,
    in_container: bool,
}

fn main() {
    if gtk::init().is_err() {
        eprintln!("agent-observer: failed to initialise GTK");
        std::process::exit(1);
    }

    let cfg = Rc::new(RefCell::new(Config::load()));

    // Screen geometry (primary monitor); the bar docks to the top or bottom edge.
    let display = gdk::Display::default().expect("no display");
    let monitor = display
        .primary_monitor()
        .or_else(|| display.monitor(0))
        .expect("no monitor");
    let geo = monitor.geometry();
    let screen_w = geo.width();
    let geo_x = geo.x();
    let geo_y = geo.y();
    let geo_h = geo.height();

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("agent-observer");
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_type_hint(gdk::WindowTypeHint::Dock);
    window.set_keep_above(true);
    window.stick();
    window.set_accept_focus(false);

    let height = cfg.borrow().bar_height;
    // Force the bar to exactly bar_height: a resizable(false) dock otherwise
    // shrinks to its content's natural height, so set_default_size alone has
    // no visible effect. A minimum size request pins it.
    window.set_size_request(screen_w, height);
    window.set_default_size(screen_w, height);
    window.move_(geo.x(), geo.y());

    // Styling.
    let provider = gtk::CssProvider::new();
    let screen = gdk::Screen::default().expect("no screen");
    gtk::StyleContext::add_provider_for_screen(
        &screen,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    apply_css(&provider, &cfg.borrow());

    // Row container.
    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row_box.set_widget_name("rowbox");
    row_box.set_size_request(screen_w, height);
    window.add(&row_box);

    // Apply size, position and reserved strut to the bar. Re-runnable so a
    // config reload can change height/position (not just a restart). Acts only
    // while the bar is visible — rebuild calls it right after showing the window
    // (visible ⟹ realized, so the X window id needed for the strut exists).
    let apply_geometry = {
        let window = window.clone();
        let row_box = row_box.clone();
        let cfg = cfg.clone();
        move || {
            if !window.is_visible() {
                return;
            }
            let (h, bottom) = {
                let c = cfg.borrow();
                (c.bar_height, is_bottom(&c))
            };
            let y = if bottom { geo_y + geo_h - h } else { geo_y };
            window.set_size_request(screen_w, h);
            row_box.set_size_request(screen_w, h);
            window.resize(screen_w, h);
            window.move_(geo_x, y);
            if let Some(xid) = window_xid(&window) {
                if let Err(e) = set_strut(xid, h, screen_w, geo_x, bottom) {
                    eprintln!("agent-observer: could not set strut: {e}");
                }
            }
        }
    };

    let cache: Rc<RefCell<TitleCache>> = Rc::new(RefCell::new(TitleCache::default()));
    // Status dots of `busy` sessions, refreshed on every rebuild, animated by the pulse timer.
    let busy_dots: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    // Sessions in displayed order, for the jump shortcut.
    let nav: Rc<RefCell<Vec<NavTarget>>> = Rc::new(RefCell::new(Vec::new()));

    let rebuild = {
        let row_box = row_box.clone();
        let cache = cache.clone();
        let cfg = cfg.clone();
        let busy_dots = busy_dots.clone();
        let nav = nav.clone();
        let provider = provider.clone();
        let window = window.clone();
        let apply_geometry = apply_geometry.clone();
        move || {
            let cfg_ref = cfg.borrow();
            let (sessions, usage) = cache.borrow_mut().scan();

            for child in row_box.children() {
                row_box.remove(&child);
            }
            busy_dots.borrow_mut().clear();
            nav.borrow_mut().clear();

            // When configured, hide the whole bar (and release its reserved
            // strut) while no sessions run, so the screen edge is given back.
            if cfg_ref.hide_when_empty && sessions.is_empty() {
                if window.is_visible() {
                    if let Some(xid) = window_xid(&window) {
                        let _ = set_strut(xid, 0, screen_w, geo_x, false);
                    }
                    window.hide();
                }
                return;
            }

            if sessions.is_empty() {
                let empty = gtk::Label::new(Some("No active Claude sessions"));
                empty.style_context().add_class("empty");
                empty.set_margin_start(10);
                row_box.add(&empty);
            } else {
                let last = sessions.len() - 1;
                for (idx, s) in sessions.iter().enumerate() {
                    let row = build_row(s, &cfg_ref, &busy_dots, idx, idx != last);
                    row_box.add(&row);
                    nav.borrow_mut().push(NavTarget {
                        host_pid: s.host_pid,
                        project: s.project().to_string(),
                        in_container: s.in_container,
                    });
                }
            }

            // Rate-limit bars pinned to the far right (independent of sessions).
            if cfg_ref.show_usage {
                if let Some(u) = usage {
                    if let Some(w) = build_usage(&u, &cfg_ref) {
                        row_box.pack_end(&w, false, false, 10);
                    }
                }
            }

            row_box.show_all();

            // Bring the bar back (and re-reserve its strut) if it had been
            // hidden while empty. apply_geometry also (re)applies size/position.
            if !window.is_visible() {
                window.show();
                apply_geometry();
            }

            // Reload styling in case config was reloaded via the menu.
            apply_css(&provider, &cfg_ref);
        }
    };

    rebuild();

    // Poll the registry.
    {
        let rebuild = rebuild.clone();
        let interval = cfg.borrow().poll_interval_secs.max(1);
        glib::timeout_add_local(Duration::from_secs(interval), move || {
            rebuild();
            glib::ControlFlow::Continue
        });
    }

    // Pulse busy dots.
    {
        let busy_dots = busy_dots.clone();
        let cfg = cfg.clone();
        let dim = Rc::new(RefCell::new(false));
        glib::timeout_add_local(Duration::from_millis(650), move || {
            if cfg.borrow().pulse_busy {
                let mut d = dim.borrow_mut();
                *d = !*d;
                let opacity = if *d { 0.4 } else { 1.0 };
                for dot in busy_dots.borrow().iter() {
                    dot.set_opacity(opacity);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Reload config from disk and re-apply everything: geometry (height/strut),
    // styling, and rows.
    let reload = {
        let cfg = cfg.clone();
        let apply_geometry = apply_geometry.clone();
        let rebuild = rebuild.clone();
        move || {
            *cfg.borrow_mut() = Config::load();
            apply_geometry();
            rebuild();
        }
    };

    // Right-click anywhere on the bar: context menu.
    {
        let reload = reload.clone();
        window.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
        window.connect_button_press_event(move |_w, ev| {
            if ev.button() == 3 {
                show_menu(ev, &reload);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        glib::Propagation::Proceed
    });

    // Visibility (and the strut/geometry that go with it) is owned by `rebuild`,
    // which was already called once above: it shows + struts the bar when there
    // are sessions, and leaves it hidden when empty (if hide_when_empty).

    // Global two-step jump shortcut: prefix, then a digit.
    {
        let sc = cfg.borrow().shortcut.clone();
        if sc.enabled {
            let nav = nav.clone();
            let jump: Rc<dyn Fn(usize)> = Rc::new(move |idx| {
                if let Some(t) = nav.borrow().get(idx) {
                    focus_session(t.host_pid, &t.project, t.in_container);
                }
            });
            if let Err(e) = hotkey::setup(&sc.prefix, jump) {
                eprintln!("agent-observer: jump shortcut disabled: {e}");
            }
        }
    }

    // Re-render immediately when the focused window changes, so the focus
    // highlight doesn't lag behind by up to one poll interval.
    {
        let rebuild = rebuild.clone();
        let on_change: Rc<dyn Fn()> = Rc::new(move || rebuild());
        if let Err(e) = active_watch::setup(on_change) {
            eprintln!("agent-observer: active-window watch disabled: {e}");
        }
    }

    gtk::main();
}

/// Build one clickable session row: status dot + formatted label. When
/// `divider` is set, a right-hand separator border is drawn — as part of this
/// cell's clickable area, so there are no dead gaps between sessions.
fn build_row(
    s: &Session,
    cfg: &Config,
    busy_dots: &Rc<RefCell<Vec<gtk::Widget>>>,
    idx: usize,
    divider: bool,
) -> gtk::EventBox {
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    hbox.set_margin_start(8);
    hbox.set_margin_end(8);

    // Draw the status dot with cairo rather than as a `●` glyph: a glyph is
    // baseline-positioned inside its font line box, so it can't be made to sit
    // dead-center in the bar. A fixed-size DrawingArea with valign Center is
    // strictly centered, and the circle is drawn centered within it.
    let diam = cfg.status_dot_size.max(1) as i32;
    let dot = gtk::DrawingArea::new();
    dot.set_size_request(diam, diam);
    dot.set_valign(gtk::Align::Center);
    let rgba = status_color(&s.status, cfg)
        .parse::<gdk::RGBA>()
        .unwrap_or_else(|_| gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
    dot.connect_draw(move |w, cr| {
        let alloc = w.allocation();
        let r = (alloc.width().min(alloc.height()) as f64) / 2.0;
        cr.arc(
            alloc.width() as f64 / 2.0,
            alloc.height() as f64 / 2.0,
            r,
            0.0,
            std::f64::consts::TAU,
        );
        cr.set_source_rgba(rgba.red(), rgba.green(), rgba.blue(), rgba.alpha());
        let _ = cr.fill();
        glib::Propagation::Proceed
    });
    hbox.add(&dot);
    if s.status == "busy" {
        busy_dots.borrow_mut().push(dot.upcast::<gtk::Widget>());
    }

    let label = gtk::Label::new(None);
    label.set_markup(&render_label(s, cfg, idx));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    hbox.add(&label);

    let row = gtk::EventBox::new();
    row.style_context().add_class("row");
    if divider {
        row.style_context().add_class("divider");
    }
    row.add(&hbox);
    row.set_tooltip_text(Some(&format!(
        "{}{}\nstatus: {}   up: {}   pid: {}",
        s.cwd,
        if s.in_container { "  (devcontainer)" } else { "" },
        s.status,
        s.uptime(),
        s.host_pid
    )));

    let host_pid = s.host_pid;
    let project = s.project().to_string();
    let in_container = s.in_container;
    row.connect_button_press_event(move |_w, ev| {
        if ev.button() == 1 {
            focus_session(host_pid, &project, in_container);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    row
}

/// Build the far-right usage cluster: one horizontal bar per present window
/// (5h, weekly). Returns `None` when there is nothing to show — no windows in
/// the data, or the capture is older than `usage_max_age_secs`.
fn build_usage(u: &Usage, cfg: &Config) -> Option<gtk::Widget> {
    if cfg.usage_max_age_secs > 0
        && u.captured_at > 0
        && now_secs().saturating_sub(u.captured_at) > cfg.usage_max_age_secs
    {
        return None;
    }

    // Outer box fills the full bar height so its left border (the separator)
    // spans top-to-bottom, matching the inter-session dividers. The bars sit in
    // an inner, vertically-centered box.
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    outer.style_context().add_class("usage-box");

    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    inner.set_valign(gtk::Align::Center);

    let mut any = false;
    if let Some(w) = &u.rate_limits.five_hour {
        inner.add(&build_usage_bar(&cfg.usage_label_5h, w.used_percentage, cfg));
        any = true;
    }
    if let Some(w) = &u.rate_limits.seven_day {
        inner.add(&build_usage_bar(&cfg.usage_label_7d, w.used_percentage, cfg));
        any = true;
    }

    outer.add(&inner);
    any.then(|| outer.upcast::<gtk::Widget>())
}

/// One labelled bar: `<label> [▓▓▓░░] NN%`. The fill is a fixed-width child of
/// a track box; its CSS class (color) is chosen from the warn/crit thresholds.
fn build_usage_bar(label: &str, pct: f64, cfg: &Config) -> gtk::Widget {
    let pct = pct.clamp(0.0, 100.0);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    bar.set_valign(gtk::Align::Center);

    let name = gtk::Label::new(None);
    name.set_markup(&format!(
        "<span size='small' alpha='65%'>{}</span>",
        glib::markup_escape_text(label)
    ));
    bar.add(&name);

    // Inset the track within the bar height, with a small floor for tiny bars.
    let bar_h = (cfg.bar_height - 14).max(6);
    let width = cfg.usage_bar_width.max(8);

    let track = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    track.set_size_request(width, bar_h);
    track.set_valign(gtk::Align::Center);
    track.style_context().add_class("usage-track");

    let fill_px = ((width as f64) * pct / 100.0).round() as i32;
    let fill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    fill.set_size_request(fill_px, bar_h);
    fill.style_context().add_class("usage-fill");
    fill.style_context().add_class(usage_level_class(pct, cfg));
    track.pack_start(&fill, false, false, 0);
    bar.add(&track);

    let val = gtk::Label::new(None);
    val.set_markup(&format!("<span size='small' alpha='80%'>{pct:.0}%</span>"));
    bar.add(&val);

    bar.upcast::<gtk::Widget>()
}

/// Pick the fill color class from the configured thresholds.
fn usage_level_class(pct: f64, cfg: &Config) -> &'static str {
    if pct >= cfg.usage_crit_pct {
        "usage-high"
    } else if pct >= cfg.usage_warn_pct {
        "usage-med"
    } else {
        "usage-low"
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Render a session's label from the configurable `label_format` template.
/// The template is Pango markup; substituted field VALUES are escaped, except
/// `{dc}` (a pre-styled marker) and `{idx}` (a bracketed digit).
fn render_label(s: &Session, cfg: &Config, idx: usize) -> String {
    let esc = |v: &str| glib::markup_escape_text(v).to_string();

    let title = s.title.as_deref().map(|t| {
        if t.chars().count() > cfg.max_title_len {
            t.chars().take(cfg.max_title_len).collect::<String>() + "\u{2026}"
        } else {
            t.to_string()
        }
    });

    // idx: [1]..[9] then [0] for the tenth; empty beyond.
    let idx_str = match idx {
        0..=8 => format!("[{}]", idx + 1),
        9 => "[0]".to_string(),
        _ => String::new(),
    };

    let dc = if s.in_container {
        " <span size='x-small' alpha='55%'>\u{2b22}dc</span>".to_string()
    } else {
        String::new()
    };

    // The focused session's project name is shown in the configured color.
    let project = {
        let p = esc(s.project());
        if s.focused {
            format!("<span foreground='{}'>{p}</span>", cfg.colors.focused)
        } else {
            p
        }
    };

    let body = render_template(&cfg.label_format, |key| match key {
        "project" => Some(project.clone()),
        "title" => Some(esc(title.as_deref().unwrap_or(""))),
        "status" => Some(esc(&s.status)),
        "uptime" => Some(esc(&s.uptime())),
        "pid" => Some(s.host_pid.to_string()),
        "cwd" => Some(esc(&s.cwd)),
        "dc" => Some(dc.clone()),
        "idx" => Some(idx_str.clone()),
        _ => None,
    });

    // Wrap in the base text color via markup (NOT CSS): GTK3 lets a CSS `color`
    // on a label override per-run Pango `foreground`, so the focused-name color
    // would be ignored. With the base color also in markup, the inner focused
    // span wins by normal Pango nesting.
    format!("<span foreground='{}'>{body}</span>", cfg.colors.text)
}

/// Substitute `{key}` tokens in `fmt` using `lookup`. Unknown keys are left
/// verbatim, so arbitrary field text containing braces can't inject tokens.
fn render_template(fmt: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        if let Some(rel) = rest[open + 1..].find('}') {
            let key = &rest[open + 1..open + 1 + rel];
            match lookup(key) {
                Some(v) => out.push_str(&v),
                None => {
                    out.push('{');
                    out.push_str(key);
                    out.push('}');
                }
            }
            rest = &rest[open + 1 + rel + 1..];
        } else {
            out.push_str(&rest[open..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// The configured color for a session status (used to fill the status dot).
fn status_color<'a>(status: &str, cfg: &'a Config) -> &'a str {
    match status {
        "busy" => &cfg.colors.busy,
        "idle" => &cfg.colors.idle,
        "waiting" => &cfg.colors.waiting,
        "interrupted" => &cfg.colors.interrupted,
        _ => &cfg.colors.unknown,
    }
}

/// Generate and load the CSS for the current config.
fn apply_css(provider: &gtk::CssProvider, cfg: &Config) {
    let c = &cfg.colors;
    let family = &cfg.font_family;
    let size = cfg.font_size;
    // The line faces the screen interior: bottom of a top-docked bar, top of a
    // bottom-docked one.
    let edge = if is_bottom(cfg) { "top" } else { "bottom" };
    let css = format!(
        "window {{ background-color: {bg}; }}\n\
         /* No text `color` here on purpose: the base color is applied via Pango\n\
            markup in render_label so the focused-name color can override it. */\n\
         #rowbox {{ background-color: {bg}; border-{edge}: {line_w}px solid {line}; }}\n\
         label {{ font-family: {family}; font-size: {size}pt; }}\n\
         .row:hover {{ background-color: alpha({text}, 0.10); }}\n\
         .divider {{ border-right: {sep_w}px solid {line}; }}\n\
         .usage-box {{ border-left: {sep_w}px solid {line}; padding-left: 8px; }}\n\
         .empty {{ color: {unknown}; }}\n\
         .usage-track {{ background-color: {usage_track}; border-radius: 2px; }}\n\
         .usage-fill {{ border-radius: 2px; }}\n\
         .usage-low {{ background-color: {usage_low}; }}\n\
         .usage-med {{ background-color: {usage_med}; }}\n\
         .usage-high {{ background-color: {usage_high}; }}\n",
        bg = c.background,
        text = c.text,
        family = family,
        size = size,
        line = c.line,
        line_w = cfg.line_width.max(0),
        sep_w = cfg.separator_width.max(0),
        edge = edge,
        unknown = c.unknown,
        usage_track = c.usage_track,
        usage_low = c.usage_low,
        usage_med = c.usage_med,
        usage_high = c.usage_high,
    );
    if let Err(e) = provider.load_from_data(css.as_bytes()) {
        eprintln!("agent-observer: CSS error: {e}");
    }
}

fn show_menu(ev: &gdk::EventButton, reload: &(impl Fn() + 'static + Clone)) {
    let menu = gtk::Menu::new();

    let reload_item = gtk::MenuItem::with_label("Reload config");
    {
        let reload = reload.clone();
        reload_item.connect_activate(move |_| reload());
    }
    menu.append(&reload_item);

    let quit = gtk::MenuItem::with_label("Quit");
    quit.connect_activate(|_| gtk::main_quit());
    menu.append(&quit);

    menu.show_all();
    menu.popup_at_pointer(Some(ev));
}

/// Reserve `height` px on the top (or bottom, if `bottom`) screen edge via
/// `_NET_WM_STRUT` / `_NET_WM_STRUT_PARTIAL`. A `height` of 0 frees the space.
fn set_strut(
    xid: u32,
    height: i32,
    width: i32,
    x0: i32,
    bottom: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    let (conn, _) = x11rb::connect(None)?;
    let strut = conn.intern_atom(false, b"_NET_WM_STRUT")?.reply()?.atom;
    let strut_partial = conn.intern_atom(false, b"_NET_WM_STRUT_PARTIAL")?.reply()?.atom;

    let h = height.max(0) as u32;
    let (top, bot) = if bottom { (0, h) } else { (h, 0) };

    // left, right, top, bottom
    let s = [0u32, 0, top, bot];
    // `.check()` round-trips so the request is processed before this short-lived
    // connection is dropped — otherwise a reload's update can be lost in the race.
    conn.change_property32(PropMode::REPLACE, xid, strut, AtomEnum::CARDINAL, &s)?
        .check()?;

    let start = x0.max(0) as u32;
    let end = (x0 + width - 1).max(0) as u32;
    // left, right, top, bottom, l_start_y, l_end_y, r_start_y, r_end_y,
    // top_start_x, top_end_x, bottom_start_x, bottom_end_x
    let mut sp = [0u32; 12];
    sp[2] = top;
    sp[3] = bot;
    let (sx, ex) = if bottom { (10, 11) } else { (8, 9) };
    sp[sx] = start;
    sp[ex] = end;
    conn.change_property32(PropMode::REPLACE, xid, strut_partial, AtomEnum::CARDINAL, &sp)?
        .check()?;

    Ok(())
}

/// Whether the bar is configured to dock to the bottom edge (default: top).
fn is_bottom(cfg: &Config) -> bool {
    cfg.position.eq_ignore_ascii_case("bottom")
}

/// The X11 window id of a realized GTK window, if it has one.
fn window_xid(window: &gtk::Window) -> Option<u32> {
    window
        .window()
        .and_then(|w| w.downcast::<gdkx11::X11Window>().ok())
        .map(|x11| x11.xid() as u32)
}
