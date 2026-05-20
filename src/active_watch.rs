//! Watches the X11 `_NET_ACTIVE_WINDOW` root property and fires a callback the
//! instant the focused window changes, so the focus highlight updates
//! immediately instead of waiting for the next poll tick.

use std::os::unix::io::AsRawFd;
use std::rc::Rc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt, EventMask};
use x11rb::protocol::Event;

/// Call `on_change` whenever the active window changes.
pub fn setup(on_change: Rc<dyn Fn()>) -> Result<(), String> {
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("x11 connect failed: {e}"))?;
    let root = conn.setup().roots[screen_num].root;

    let active_atom = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")
        .map_err(|e| format!("intern_atom failed: {e}"))?
        .reply()
        .map_err(|e| format!("intern_atom reply failed: {e}"))?
        .atom;

    // Subscribe to property changes on the root window (per-client mask).
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| format!("select PropertyChange failed: {e}"))?;
    conn.flush().map_err(|e| format!("flush failed: {e}"))?;

    let conn = Rc::new(conn);
    let fd = conn.stream().as_raw_fd();
    let c = conn.clone();
    glib::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
        let mut changed = false;
        while let Ok(Some(event)) = c.poll_for_event() {
            if let Event::PropertyNotify(p) = event {
                if p.atom == active_atom {
                    changed = true; // coalesce bursts into a single refresh
                }
            }
        }
        if changed {
            on_change();
        }
        glib::ControlFlow::Continue
    });

    Ok(())
}
