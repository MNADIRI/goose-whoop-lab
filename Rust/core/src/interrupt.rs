//! Analysis work the app runs in the foreground (packet-derived inputs, rollups) can still be
//! running when the app goes to the background; iOS kills an app above 80 % CPU over 60 s there
//! (20 Nov 11:02: a recovery rollup started in the foreground, 48 s of CPU after backgrounding).
//! Requests marked `"interruptible": true` install a SQLite progress handler on the connections
//! they open: once the app reports the background, their queries stop with SQLITE_INTERRUPT.
//! Frame ingest and history sync never set the mark, so they keep running.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::Connection;

static BACKGROUND: AtomicBool = AtomicBool::new(false);

thread_local! {
    static INTERRUPTIBLE: Cell<bool> = const { Cell::new(false) };
}

pub fn set_background(background: bool) {
    BACKGROUND.store(background, Ordering::Relaxed);
    if background {
        crate::store::clear_decoded_window_cache();
    }
}

pub fn is_background() -> bool {
    BACKGROUND.load(Ordering::Relaxed)
}

/// Marks the current thread's requests interruptible while alive.
pub struct InterruptibleScope {
    previous: bool,
}

impl InterruptibleScope {
    pub fn enter() -> Self {
        let previous = INTERRUPTIBLE.with(|c| c.replace(true));
        Self { previous }
    }
}

impl Drop for InterruptibleScope {
    fn drop(&mut self) {
        INTERRUPTIBLE.with(|c| c.set(self.previous));
    }
}

/// Called on every connection the store opens.
pub fn install(conn: &Connection) {
    if INTERRUPTIBLE.with(|c| c.get()) {
        conn.progress_handler(20_000, Some(|| BACKGROUND.load(Ordering::Relaxed)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interruptible_query_stops_in_background() {
        let _scope = InterruptibleScope::enter();
        let conn = Connection::open_in_memory().unwrap();
        install(&conn);
        set_background(true);
        let r: rusqlite::Result<i64> = conn.query_row(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000) SELECT SUM(x) FROM c",
            [],
            |r| r.get(0),
        );
        set_background(false);
        assert!(r.is_err());
        let ok: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, 1);
    }
}
