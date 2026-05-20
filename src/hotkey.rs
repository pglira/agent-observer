//! Global two-step jump shortcut (tmux-style): press the configured prefix
//! (e.g. `ctrl+b`), then a digit `1..9`/`0` to focus the Nth visible session.
//!
//! The bar is a focus-less dock window, so we grab keys globally on the X11
//! root window via a dedicated x11rb connection, whose fd is driven by the
//! GLib main loop.

use std::cell::Cell;
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt, GrabMode, Keycode, ModMask,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// Lock-style modifiers we must add to the grab so it still fires while
/// NumLock / CapsLock are on (Mod2 is typically NumLock, Lock is CapsLock).
const LOCK_VARIANTS: [u16; 4] = [
    0,
    1 << 1, // Lock (CapsLock)
    1 << 4, // Mod2 (NumLock)
    (1 << 1) | (1 << 4),
];

struct State {
    conn: RustConnection,
    /// keycode -> session index (0-based) for digit keys 1..9,0.
    digits: HashMap<Keycode, usize>,
    armed: Cell<bool>,
    /// generation counter so a stale disarm-timeout is a no-op.
    generation: Cell<u64>,
}

/// Set up the global shortcut. `jump(idx)` focuses the idx-th visible session.
/// Returns Err with a human message if the prefix can't be parsed/grabbed.
pub fn setup(prefix: &str, jump: Rc<dyn Fn(usize)>) -> Result<(), String> {
    let (mods, keysym) = parse_combo(prefix)
        .ok_or_else(|| format!("could not parse shortcut prefix '{prefix}'"))?;

    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("x11 connect failed: {e}"))?;
    let root = conn.setup().roots[screen_num].root;

    // Resolve keysym -> keycode using the server's keyboard map.
    let setup = conn.setup();
    let min = setup.min_keycode;
    let count = setup.max_keycode - min + 1;
    let map = conn
        .get_keyboard_mapping(min, count)
        .and_then(|c| Ok(c.reply()))
        .map_err(|e| format!("keyboard mapping failed: {e}"))?
        .map_err(|e| format!("keyboard mapping reply failed: {e}"))?;
    let per = map.keysyms_per_keycode as usize;
    let resolve = |target: u32| -> Option<Keycode> {
        for kc in min..=setup.max_keycode {
            let base = (kc - min) as usize * per;
            for off in 0..per {
                if map.keysyms.get(base + off).copied() == Some(target) {
                    return Some(kc);
                }
            }
        }
        None
    };

    let prefix_kc =
        resolve(keysym).ok_or_else(|| format!("no keycode for prefix '{prefix}'"))?;

    // Map digit keys 1..9 then 0 to indices 0..9.
    let mut digits = HashMap::new();
    for (i, sym) in (0x31..=0x39).chain(std::iter::once(0x30)).enumerate() {
        if let Some(kc) = resolve(sym) {
            digits.insert(kc, i);
        }
    }

    // Passively grab the prefix combo (incl. lock-modifier variants).
    for extra in LOCK_VARIANTS {
        let m = ModMask::from(u16::from(mods) | extra);
        conn.grab_key(true, root, m, prefix_kc, GrabMode::ASYNC, GrabMode::ASYNC)
            .map_err(|e| format!("grab_key failed: {e}"))?;
    }
    conn.flush().map_err(|e| format!("flush failed: {e}"))?;

    let state = Rc::new(State {
        conn,
        digits,
        armed: Cell::new(false),
        generation: Cell::new(0),
    });

    let fd = state.conn.stream().as_raw_fd();
    let st = state.clone();
    glib::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
        drain_events(&st, root, &jump);
        glib::ControlFlow::Continue
    });

    Ok(())
}

fn drain_events(st: &Rc<State>, root: u32, jump: &Rc<dyn Fn(usize)>) {
    while let Ok(Some(event)) = st.conn.poll_for_event() {
        let Event::KeyPress(ev) = event else { continue };

        if !st.armed.get() {
            // Idle: the only grabbed key is the prefix → arm and grab keyboard.
            arm(st, root);
        } else {
            // Armed: this is the second keystroke.
            if let Some(&idx) = st.digits.get(&ev.detail) {
                jump(idx);
            }
            disarm(st);
        }
    }
}

fn arm(st: &Rc<State>, root: u32) {
    let _ = st.conn.grab_keyboard(
        true,
        root,
        0u32,
        GrabMode::ASYNC,
        GrabMode::ASYNC,
    );
    let _ = st.conn.flush();
    st.armed.set(true);
    let gen = st.generation.get().wrapping_add(1);
    st.generation.set(gen);

    // Auto-disarm if no digit is pressed within 2s.
    let st2 = st.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(2000), move || {
        if st2.armed.get() && st2.generation.get() == gen {
            disarm(&st2);
        }
    });
}

fn disarm(st: &Rc<State>) {
    let _ = st.conn.ungrab_keyboard(0u32);
    let _ = st.conn.flush();
    st.armed.set(false);
}

/// Parse "ctrl+shift+b" -> (modmask, keysym). Single-character keys map to
/// their ASCII/Latin-1 keysym (covers letters and digits).
fn parse_combo(s: &str) -> Option<(ModMask, u32)> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    let (key, mod_parts) = parts.split_last()?;

    let mut m: u16 = 0;
    for p in mod_parts {
        m |= match p.to_lowercase().as_str() {
            "ctrl" | "control" => u16::from(ModMask::CONTROL),
            "shift" => u16::from(ModMask::SHIFT),
            "alt" | "mod1" => u16::from(ModMask::M1),
            "super" | "win" | "mod4" | "cmd" => u16::from(ModMask::M4),
            _ => return None,
        };
    }

    let keysym = key_to_keysym(key)?;
    Some((ModMask::from(m), keysym))
}

fn key_to_keysym(key: &str) -> Option<u32> {
    let lower = key.to_lowercase();
    let mut chars = lower.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // only single-character keys supported
    }
    if c.is_ascii_graphic() {
        Some(c as u32) // Latin-1 keysyms equal ASCII for printable range
    } else {
        None
    }
}
