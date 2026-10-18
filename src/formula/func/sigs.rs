//! The signature hint shown while a formula is typed (`SUMIF(range,
//! criteria, [sum_range])`), from each function's
//! [`sig`](super::FuncDef::sig). Optional arguments are bracketed; `…`
//! repeats the one before it.

pub use super::signature;

/// `NAME(args)` with argument `index` (0-based) marked `«like_this»`; a
/// variadic tail marks its repeating argument.
pub fn hint(name: &str, index: usize) -> Option<String> {
    let sig = signature(name)?;
    if sig.is_empty() {
        return Some(format!("{name}()"));
    }
    let parts: Vec<&str> = sig.split(", ").collect();
    let repeat = parts.iter().position(|p| *p == "…");
    let mark = match repeat {
        // Past the listed arguments of a variadic list: mark the one that
        // repeats (the argument just before `…`).
        Some(rep) if index >= rep => rep.saturating_sub(1),
        _ => index,
    };
    let shown: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == mark {
                format!("«{p}»")
            } else {
                (*p).to_string()
            }
        })
        .collect();
    Some(format!("{name}({})", shown.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for name in super::super::all()
            .map(|f| f.name)
            .chain(super::super::SPECIAL.iter().map(|s| s.0))
        {
            assert!(seen.insert(name), "{name} is defined twice");
        }
    }

    #[test]
    fn hints_mark_the_current_argument() {
        assert_eq!(
            hint("SUMIF", 1).as_deref(),
            Some("SUMIF(range, «criteria», [sum_range])")
        );
        assert_eq!(
            hint("SUM", 5).as_deref(),
            Some("SUM(number1, «[number2]», …)")
        );
        assert_eq!(hint("PI", 0).as_deref(), Some("PI()"));
        assert_eq!(
            hint("FILTER", 1).as_deref(),
            Some("FILTER(array, «include», [if_empty])")
        );
        assert_eq!(
            hint("SORTBY", 1).as_deref(),
            Some("SORTBY(array, «by_array1», [sort_order1], [by_array2, sort_order2], …)")
        );
    }
}
