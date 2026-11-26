//! [`HexData`]: a hex buffer's bytes and its undo history.
//!
//! Every change is one splice (replace `old` at `at` with `new`), which
//! covers overwriting, inserting and deleting. Undo replays the inverse
//! splice. A run of typing (overwriting, inserting or appending, up to 64
//! bytes) coalesces into one undo step, as typing text does. Same-length splices (the common case: overwriting) copy in place, so
//! editing a large file never shifts its tail.

/// One recorded change.
#[derive(Debug, Clone)]
struct Edit {
    at: usize,
    old: Vec<u8>,
    new: Vec<u8>,
    /// Where the cursor was before the edit (undo puts it back).
    cursor: usize,
    /// Made by typing: later typed bytes next to it may join it.
    typing: bool,
}

/// The longest run of typed bytes one undo step holds.
const TYPING_RUN: usize = 64;

#[derive(Debug, Default)]
pub struct HexData {
    bytes: Vec<u8>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// The undo depth whose bytes match the file on disk; `None` once that
    /// state can no longer be reached (an edit after undoing past it).
    saved: Option<usize>,
}

impl HexData {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            undo: Vec::new(),
            redo: Vec::new(),
            saved: Some(0),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[allow(dead_code)] // part of the API (clippy pairs it with `len`).
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn get(&self, at: usize) -> Option<u8> {
        self.bytes.get(at).copied()
    }

    /// `range`, clamped to the buffer.
    pub fn slice(&self, range: std::ops::Range<usize>) -> &[u8] {
        let end = range.end.min(self.bytes.len());
        &self.bytes[range.start.min(end)..end]
    }

    /// Unsaved changes since the last load or save.
    pub fn modified(&self) -> bool {
        self.saved != Some(self.undo.len())
    }

    /// The bytes now match the file on disk.
    pub fn mark_saved(&mut self) {
        self.saved = Some(self.undo.len());
    }

    /// The file on disk no longer matches any state in the history (a
    /// write of it failed).
    pub fn forget_saved(&mut self) {
        self.saved = None;
    }

    /// Replace everything (revert): the history starts over.
    pub fn reset(&mut self, bytes: Vec<u8>) {
        *self = Self::new(bytes);
    }

    /// Replace `remove` bytes at `at` with `insert` as one undo step.
    /// `cursor` is where point was, for undo. Returns whether anything
    /// changed.
    pub fn splice(&mut self, at: usize, remove: usize, insert: &[u8], cursor: usize) -> bool {
        let at = at.min(self.bytes.len());
        let end = at.saturating_add(remove).min(self.bytes.len());
        if self.bytes[at..end] == *insert {
            return false;
        }
        let old = self.bytes[at..end].to_vec();
        self.apply(at, old.len(), insert);
        self.push(Edit {
            at,
            old,
            new: insert.to_vec(),
            cursor,
            typing: false,
        });
        true
    }

    /// Type `byte` at `at`: overwrite the byte there (appending at the end),
    /// or with `insert`, insert it. Joins the previous typed run when it
    /// continues it, so a run of typing undoes as one step.
    pub fn type_byte(&mut self, at: usize, byte: u8, insert: bool, cursor: usize) -> bool {
        let at = at.min(self.bytes.len());
        let overwrite = !insert && at < self.bytes.len();
        if overwrite && self.bytes[at] == byte {
            return false;
        }
        let at_save_point = self.saved == Some(self.undo.len());
        // Never join across a save, or after an undo (the run being
        // extended is history now).
        if !at_save_point
            && self.redo.is_empty()
            && let Some(last) = self.undo.last_mut()
            && last.typing
        {
            let end = last.at + last.new.len();
            if overwrite && (last.at..end).contains(&at) {
                // The second nibble of a byte, or retyping inside the run:
                // those bytes are the run's own.
                last.new[at - last.at] = byte;
                self.bytes[at] = byte;
                return true;
            }
            if at == end && last.new.len() < TYPING_RUN {
                if !overwrite {
                    // An insert (or append) right after the run.
                    last.new.push(byte);
                    self.bytes.insert(at, byte);
                    return true;
                }
                // Overwriting the byte after the run joins it only when the
                // run replaced as many bytes as it wrote (so `old` lines up).
                if last.old.len() == last.new.len() {
                    last.old.push(self.bytes[at]);
                    last.new.push(byte);
                    self.bytes[at] = byte;
                    return true;
                }
            }
        }
        let remove = usize::from(overwrite);
        let old = self.bytes[at..at + remove].to_vec();
        self.apply(at, remove, &[byte]);
        self.push(Edit {
            at,
            old,
            new: vec![byte],
            cursor,
            typing: true,
        });
        true
    }

    /// Undo the last change; the cursor to restore.
    pub fn undo(&mut self) -> Option<usize> {
        let e = self.undo.pop()?;
        self.apply(e.at, e.new.len(), &e.old);
        let cursor = e.cursor;
        self.redo.push(e);
        Some(cursor)
    }

    /// Redo the last undone change; the cursor to show (the change's start).
    pub fn redo(&mut self) -> Option<usize> {
        let e = self.redo.pop()?;
        self.apply(e.at, e.old.len(), &e.new);
        let at = e.at;
        self.undo.push(e);
        Some(at)
    }

    fn push(&mut self, e: Edit) {
        // The saved state was in the redo stack: dropping it makes it
        // unreachable.
        if self.saved.is_some_and(|s| s > self.undo.len()) {
            self.saved = None;
        }
        self.redo.clear();
        self.undo.push(e);
    }

    fn apply(&mut self, at: usize, remove: usize, insert: &[u8]) {
        if remove == insert.len() {
            self.bytes[at..at + remove].copy_from_slice(insert);
        } else {
            self.bytes.splice(at..at + remove, insert.iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overwrite_insert_delete_undo_redo() {
        let mut d = HexData::new(vec![1, 2, 3, 4]);
        assert!(!d.modified());
        assert!(d.splice(1, 1, &[9], 1));
        assert_eq!(d.bytes(), &[1, 9, 3, 4]);
        assert!(d.splice(2, 0, &[7, 7], 2));
        assert_eq!(d.bytes(), &[1, 9, 7, 7, 3, 4]);
        assert!(d.splice(0, 2, &[], 0));
        assert_eq!(d.bytes(), &[7, 7, 3, 4]);
        assert!(d.modified());
        assert_eq!(d.undo(), Some(0));
        assert_eq!(d.undo(), Some(2));
        assert_eq!(d.undo(), Some(1));
        assert_eq!(d.bytes(), &[1, 2, 3, 4]);
        assert!(!d.modified());
        assert_eq!(d.undo(), None);
        assert_eq!(d.redo(), Some(1));
        assert_eq!(d.bytes(), &[1, 9, 3, 4]);
    }

    #[test]
    fn no_op_splice_records_nothing() {
        let mut d = HexData::new(vec![1, 2]);
        assert!(!d.splice(0, 1, &[1], 0));
        assert!(!d.splice(2, 0, &[], 0));
        assert!(!d.modified());
        assert_eq!(d.undo(), None);
    }

    #[test]
    fn typing_coalesces_into_one_step() {
        let mut d = HexData::new(vec![0; 4]);
        // Two nibbles of byte 0, then byte 1.
        d.type_byte(0, 0xa0, false, 0);
        d.type_byte(0, 0xab, false, 0);
        d.type_byte(1, 0xcd, false, 1);
        assert_eq!(d.bytes(), &[0xab, 0xcd, 0, 0]);
        assert_eq!(d.undo(), Some(0));
        assert_eq!(d.bytes(), &[0, 0, 0, 0]);
        assert_eq!(d.undo(), None);
    }

    #[test]
    fn typing_at_the_end_appends() {
        let mut d = HexData::new(vec![1]);
        d.type_byte(1, 2, false, 1);
        assert_eq!(d.bytes(), &[1, 2]);
        d.undo();
        assert_eq!(d.bytes(), &[1]);
    }

    #[test]
    fn inserted_and_appended_typing_is_one_step() {
        let mut d = HexData::new(vec![9]);
        // Insert two hex bytes before 9, nibble by nibble.
        d.type_byte(0, 0x10, true, 0);
        d.type_byte(0, 0x12, false, 0);
        d.type_byte(1, 0x30, true, 1);
        d.type_byte(1, 0x34, false, 1);
        assert_eq!(d.bytes(), &[0x12, 0x34, 9]);
        // Append after the end, then undo it all at once.
        let mut e = HexData::new(vec![1]);
        e.type_byte(1, 2, false, 1);
        e.type_byte(2, 3, false, 2);
        assert_eq!(e.bytes(), &[1, 2, 3]);
        assert_eq!(d.undo(), Some(0));
        assert_eq!(d.bytes(), &[9]);
        assert_eq!(e.undo(), Some(1));
        assert_eq!(e.bytes(), &[1]);
    }

    #[test]
    fn typing_does_not_join_across_a_save() {
        let mut d = HexData::new(vec![0; 3]);
        d.type_byte(0, 1, false, 0);
        d.mark_saved();
        d.type_byte(1, 2, false, 1);
        assert!(d.modified());
        d.undo();
        assert!(!d.modified());
        assert_eq!(d.bytes(), &[1, 0, 0]);
    }

    #[test]
    fn saved_state_unreachable_after_branching() {
        let mut d = HexData::new(vec![0; 2]);
        d.splice(0, 1, &[1], 0);
        d.mark_saved();
        d.undo();
        assert!(d.modified());
        d.splice(1, 1, &[5], 1);
        assert!(d.modified());
        d.undo();
        assert!(d.modified()); // [0, 0] was never saved
    }
}
