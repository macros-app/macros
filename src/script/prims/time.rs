//! Identity and time primitives (org-roam node IDs, timestamps).

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, _ctx: &EngineShared) {
    reg.section(Section::TimeIds);
    // --- Identity + time (org-roam node IDs, timestamps) ------------------
    // `(org-id-new)` → a fresh v4 UUID string (org-roam node IDs).
    reg.register_fn("org-id-new", || -> String {
        uuid::Uuid::new_v4().to_string()
    });
    // `(current-time fmt)` → local time formatted with a `strftime`-style string
    // (Emacs `format-time-string`), e.g. `(current-time "%Y%m%d%H%M%S")` for a
    // node filename, `(current-time "%Y-%m-%d")` for an agenda date. An invalid
    // format yields "" rather than an error.
    reg.register_fn("current-time", |fmt: String| -> String {
        jiff::fmt::strtime::format(&fmt, &jiff::Zoned::now()).unwrap_or_default()
    });
}
