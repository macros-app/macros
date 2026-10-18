//! Persisted per-file view settings (column widths, row heights, zoom,
//! header choice, frozen rows and columns, hidden columns), kept beside the config so
//! they never touch the CSV. One JSON file per grid file, named by a hash
//! of its canonical path.

use std::path::{Path, PathBuf};

/// What is remembered about how one file's grid was laid out.
#[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
pub struct Sidecar {
    pub cols: Vec<(usize, f64)>,
    pub rows: Vec<(usize, f64)>,
    pub zoom: Option<f32>,
    pub header: Option<bool>,
    /// Frozen rows, when the user froze some (`grid-toggle-freeze`).
    #[serde(default)]
    pub freeze: Option<usize>,
    /// Frozen columns, when the user froze some (`grid-freeze-panes`).
    #[serde(default)]
    pub freeze_cols: Option<usize>,
    /// Columns hidden with `grid-hide-column`.
    #[serde(default)]
    pub hidden_cols: Vec<usize>,
}

impl Sidecar {
    /// The settings saved for `file`, if any (a missing or unreadable
    /// sidecar is no settings).
    pub fn load(file: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path_for(file)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Save these settings for `file` (best effort: a failure only loses
    /// the layout).
    pub fn store(&self, file: &Path) {
        let path = path_for(file);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// Where `file`'s sidecar lives.
fn path_for(file: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let key = super::bytes_hash(canon.to_string_lossy().as_bytes());
    crate::runtime::config_directory()
        .join("grid-layouts")
        .join(format!("{key:016x}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_rows_and_columns_round_trip() {
        let sc = Sidecar {
            freeze: Some(1),
            freeze_cols: Some(2),
            ..Sidecar::default()
        };
        let text = serde_json::to_string(&sc).unwrap();
        assert_eq!(serde_json::from_str::<Sidecar>(&text).unwrap(), sc);
    }

    #[test]
    fn old_sidecars_without_newer_fields_still_load() {
        let sc: Sidecar =
            serde_json::from_str(r#"{"cols":[[2,90.0]],"rows":[],"zoom":null,"header":true}"#)
                .unwrap();
        assert_eq!(
            sc,
            Sidecar {
                cols: vec![(2, 90.0)],
                header: Some(true),
                ..Sidecar::default()
            }
        );
    }
}
