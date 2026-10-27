//! Fast search: ripgrep as a library, configured from Scheme (see
//! `SearchConfig`).

use std::sync::{Arc, Mutex};
use steel::rvals::SteelVal;

use super::*;

pub(in crate::script) fn register(reg: &mut Registrar, _ctx: &EngineShared) {
    reg.section(Section::Search);
    // --- Fast search (ripgrep as a library, Scheme-configurable) ----------
    // The config lives entirely on this (engine) thread; the register_fns share
    // it. Users tune it in init.scm: (search-exclude "target"),
    // (set-search-option "hidden" "true"), etc.
    {
        let search: Arc<Mutex<SearchConfig>> = Arc::new(Mutex::new(SearchConfig::default()));
        {
            let s = search.clone();
            reg.register_fn("search-exclude", move |glob: String| {
                s.lock().unwrap().excludes.push(glob);
            });
        }
        {
            let s = search.clone();
            reg.register_fn("search-include", move |glob: String| {
                s.lock().unwrap().includes.push(glob);
            });
        }
        {
            let s = search.clone();
            reg.register_fn("search-reset", move || {
                *s.lock().unwrap() = SearchConfig::default();
            });
        }
        {
            let s = search.clone();
            reg.register_fn("set-search-option", move |key: String, value: SteelVal| {
                let v = option_from_steelval(&value);
                let mut c = s.lock().unwrap();
                match key.as_str() {
                    "hidden" => c.hidden = v.truthy(),
                    "follow-symlinks" => c.follow_links = v.truthy(),
                    "respect-gitignore" => c.respect_gitignore = v.truthy(),
                    "max-depth" => c.max_depth = v.as_usize(),
                    _ => {}
                }
            });
        }
        {
            // `(walk-files dir)` → file paths under `dir`, gitignore-aware.
            let s = search.clone();
            reg.register_fn("walk-files", move |dir: String| -> Vec<String> {
                let config = s.lock().unwrap().clone();
                build_walk(&dir, &config)
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect()
            });
        }
        {
            // `(grep-files dir regex)` → `"path:line:text"` matches.
            let s = search.clone();
            reg.register_fn(
                "grep-files",
                move |dir: String, pattern: String| -> Vec<String> {
                    let config = s.lock().unwrap().clone();
                    grep_walk(&dir, &pattern, &config).unwrap_or_default()
                },
            );
        }
        {
            // `(grep-files-in dir regex glob recursive?)` — `grep-files` for
            // M-x grep / rgrep / lgrep: only files whose name matches `glob`
            // ("" or "*" = all), and with `recursive?` #f only `dir` itself.
            // An invalid regexp is an error (grep-files returns no matches).
            let s = search.clone();
            reg.register_fn(
                "grep-files-in",
                move |dir: String,
                      pattern: String,
                      glob: String,
                      recursive: bool|
                      -> Result<Vec<String>, String> {
                    let mut config = s.lock().unwrap().clone();
                    let glob = glob.trim();
                    if !glob.is_empty() && glob != "*" {
                        config.includes = glob.split_whitespace().map(String::from).collect();
                    }
                    if !recursive {
                        config.max_depth = Some(1);
                    }
                    grep_walk(&dir, &pattern, &config)
                },
            );
        }
    }
}

/// Every `"path:line:text"` match of `pattern` under `dir` (see `build_walk`).
fn grep_walk(dir: &str, pattern: &str, config: &SearchConfig) -> Result<Vec<String>, String> {
    let matcher = grep::regex::RegexMatcher::new(pattern)
        .map_err(|e| format!("Invalid regexp {pattern:?}: {e}"))?;
    let mut searcher = grep::searcher::SearcherBuilder::new()
        .line_number(true)
        .build();
    let mut results: Vec<String> = Vec::new();
    for entry in build_walk(dir, config).filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let path_str = path.to_string_lossy().into_owned();
        let out = &mut results;
        let _ = searcher.search_path(
            &matcher,
            &path,
            grep::searcher::sinks::UTF8(move |lnum, line| {
                out.push(format!("{}:{}:{}", path_str, lnum, line.trim_end()));
                Ok(true)
            }),
        );
    }
    Ok(results)
}
