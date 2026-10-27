//! Markers: positions that move with edits.

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, ctx: &EngineShared) {
    let EngineShared {
        markers, snapshot, ..
    } = ctx;
    reg.section(Section::Markers);
    // --- Markers (positions that move with edits) -------------------------
    {
        // `(make-marker pos)` → a marker id (allocated synchronously).
        let m = markers.clone();
        reg.register_fn("make-marker", move |pos: isize| -> usize {
            m.lock().unwrap().alloc(scheme_pos(pos)) as usize
        });
    }
    {
        // `(make-buffer-marker pos)` → a marker id bound to the current buffer.
        // A plain marker is dropped when another buffer becomes active; this
        // one parks at its last position and resumes tracking edits when its
        // buffer is active again (Emacs markers always remember their buffer).
        // Point registers (`C-x r SPC`) use these.
        let m = markers.clone();
        let shared = snapshot.clone();
        reg.register_fn("make-buffer-marker", move |pos: isize| -> usize {
            let mut store = m.lock().unwrap();
            let id = store.alloc(scheme_pos(pos));
            store.owners.insert(id, shared.load().name.clone());
            id as usize
        });
    }
    {
        // `(marker-position id)` → its current byte position, or -1.
        let m = markers.clone();
        reg.register_fn("marker-position", move |NonNeg64(id): NonNeg64| -> isize {
            m.lock().unwrap().position(id).map_or(-1, |p| p as isize)
        });
    }
    {
        let m = markers.clone();
        reg.register_fn("set-marker", move |NonNeg64(id): NonNeg64, pos: isize| {
            let mut store = m.lock().unwrap();
            // A parked marker stays parked (its buffer isn't the active one).
            match store.parked.get_mut(&id) {
                Some(p) => *p = scheme_pos(pos),
                None => {
                    store.positions.insert(id, scheme_pos(pos));
                }
            }
        });
    }
    {
        let m = markers.clone();
        reg.register_fn("delete-marker", move |NonNeg64(id): NonNeg64| {
            m.lock().unwrap().remove(id);
        });
    }
}
