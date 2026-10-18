//! Argument helpers shared by the category files: 2-D views over range and
//! array arguments, flattening, and the date-system-aware coercions.

use std::borrow::Cow;

use super::{Arg, Array, Ctx, RangeRef};
use crate::formula::date;
use crate::formula::value::{ErrorKind, Value};

/// Most cells a formula may expand into one dense array (about 8.4M: four
/// full Excel columns). Past it the result is `#NUM!` rather than an
/// allocation of billions of values that aborts the whole editor.
pub const MAX_ARRAY_CELLS: usize = 1 << 23;

/// `rows × cols` when it fits under [`MAX_ARRAY_CELLS`], else `#NUM!`.
pub fn dense_cells(rows: usize, cols: usize) -> Result<usize, ErrorKind> {
    rows.checked_mul(cols)
        .filter(|&n| n <= MAX_ARRAY_CELLS)
        .ok_or(ErrorKind::Num)
}

/// A rectangular argument: a (clamped) range, an array, or a lone value
/// standing in as a 1×1 area.
#[derive(Clone, Debug)]
pub enum Area<'a> {
    Range(RangeRef),
    Array(Cow<'a, Array>),
    Scalar(Value),
}

/// View an argument as an area. Ranges are clamped to the used area where
/// open-ended; a missing sheet is `#REF!`.
pub fn area<'a>(arg: &'a Arg, ctx: &dyn Ctx) -> Result<Area<'a>, ErrorKind> {
    Ok(match arg {
        Arg::Range(r) => Area::Range(ctx.clamp(r).ok_or(ErrorKind::Ref)?),
        Arg::Array(a) => Area::Array(Cow::Borrowed(a)),
        Arg::Value(v) => Area::Scalar(v.clone()),
        Arg::Omitted => Area::Scalar(Value::Empty),
    })
}

impl Area<'_> {
    pub fn rows(&self) -> usize {
        match self {
            Area::Range(r) => r.rows(),
            Area::Array(a) => a.rows,
            Area::Scalar(_) => 1,
        }
    }

    pub fn cols(&self) -> usize {
        match self {
            Area::Range(r) => r.cols(),
            Area::Array(a) => a.cols,
            Area::Scalar(_) => 1,
        }
    }

    pub fn len(&self) -> usize {
        self.rows() * self.cols()
    }

    /// One row or one column.
    pub fn is_vector(&self) -> bool {
        self.rows() == 1 || self.cols() == 1
    }

    /// The value at `(r, c)` (0-based, in bounds).
    pub fn get(&self, ctx: &dyn Ctx, r: usize, c: usize) -> Value {
        match self {
            Area::Range(rg) => ctx.cell(rg.sheet.as_deref(), rg.r0 + r, rg.c0 + c),
            Area::Array(a) => a.get(r, c).clone(),
            Area::Scalar(v) => v.clone(),
        }
    }

    /// Element `i` of a vector (down a column, or along a row).
    pub fn vget(&self, ctx: &dyn Ctx, i: usize) -> Value {
        if self.cols() == 1 {
            self.get(ctx, i, 0)
        } else {
            self.get(ctx, 0, i)
        }
    }

    /// The `rows`×`cols` sub-area at `(r0, c0)`. For a range it may extend
    /// past the original (Excel's `SUMIF` sum-range resizing).
    pub fn sub(&self, r0: usize, c0: usize, rows: usize, cols: usize) -> Area<'static> {
        match self {
            Area::Range(rg) => Area::Range(RangeRef {
                sheet: rg.sheet.clone(),
                r0: rg.r0 + r0,
                c0: rg.c0 + c0,
                r1: rg.r0 + r0 + rows.max(1) - 1,
                c1: rg.c0 + c0 + cols.max(1) - 1,
            }),
            Area::Array(a) => {
                if dense_cells(rows, cols).is_err() {
                    return Area::Scalar(Value::Error(ErrorKind::Num));
                }
                let mut data = Vec::with_capacity(rows * cols);
                for r in r0..r0 + rows {
                    for c in c0..c0 + cols {
                        data.push(if r < a.rows && c < a.cols {
                            a.get(r, c).clone()
                        } else {
                            Value::Error(ErrorKind::NA)
                        });
                    }
                }
                Area::Array(Cow::Owned(Array { rows, cols, data }))
            }
            Area::Scalar(v) => Area::Scalar(v.clone()),
        }
    }

    /// The values of the top-left `rows`×`cols` window, row-major, reading
    /// only the used part of a range (everything else is empty).
    pub fn window(&self, ctx: &dyn Ctx, rows: usize, cols: usize) -> Vec<Value> {
        let mut out = vec![Value::Empty; rows * cols];
        match self {
            Area::Range(rg) => {
                let win = RangeRef {
                    sheet: rg.sheet.clone(),
                    r0: rg.r0,
                    c0: rg.c0,
                    r1: rg.r0 + rows.max(1) - 1,
                    c1: rg.c0 + cols.max(1) - 1,
                };
                if rows == 0 || cols == 0 {
                    return out;
                }
                if let Some(part) = ctx.data_part(&win) {
                    ctx.for_each(&part, &mut |r, c, v| {
                        out[(r - rg.r0) * cols + (c - rg.c0)] = v;
                    });
                }
            }
            _ => {
                for r in 0..rows.min(self.rows()) {
                    for c in 0..cols.min(self.cols()) {
                        out[r * cols + c] = self.get(ctx, r, c);
                    }
                }
            }
        }
        out
    }

    /// All values, row-major.
    /// `#NUM!` past [`MAX_ARRAY_CELLS`]: the whole area becomes one dense
    /// list, so an explicit `A1:Z99999999` would otherwise try to allocate
    /// billions of cells and abort the app.
    pub fn values(&self, ctx: &dyn Ctx) -> Result<Vec<Value>, ErrorKind> {
        dense_cells(self.rows(), self.cols())?;
        Ok(self.window(ctx, self.rows(), self.cols()))
    }

    /// How many leading rows and columns hold any data (the rest of the
    /// area is empty). Arrays and scalars are all data.
    pub fn data_dims(&self, ctx: &dyn Ctx) -> (usize, usize) {
        match self {
            Area::Range(rg) => match ctx.data_part(rg) {
                Some(p) => (p.r1 - rg.r0 + 1, p.c1 - rg.c0 + 1),
                None => (0, 0),
            },
            _ => (self.rows(), self.cols()),
        }
    }
}

/// `max_args` for variadic functions.
pub const MANY: usize = usize::MAX;

/// A function definition for a category's `FUNCS` table:
/// `def!("NAME", min_args, max_args, body, "signature")`, or
/// `def!(volatile "NOW", …)` for one re-run on every recalculation. `body`
/// is any
/// `fn(&[Arg], &mut dyn Ctx) -> R` where `R:` [`IntoValue`](super::IntoValue);
/// the result is converted to a cell value here, once. `def!(array
/// "SORT", …)` defines a dynamic-array function, whose body returns
/// `Result<Array, ErrorKind>`.
macro_rules! def {
    (array $name:literal, $min:expr, $max:expr, $f:path, $sig:literal) => {
        $crate::formula::func::FuncDef {
            name: $name,
            min_args: $min,
            max_args: $max,
            sig: $sig,
            f: |args: &[$crate::formula::func::Arg], ctx: &mut dyn $crate::formula::func::Ctx| {
                match $f(args, ctx) {
                    Ok(a) => a.data.into_iter().next().unwrap_or($crate::formula::value::Value::Empty),
                    Err(e) => $crate::formula::value::Value::Error(e),
                }
            },
            array: Some($f),
            volatile: false,
        }
    };
    (volatile $name:literal, $min:expr, $max:expr, $f:path, $sig:literal) => {
        $crate::formula::func::args::def!(@ $name, $min, $max, $f, $sig, true)
    };
    ($name:literal, $min:expr, $max:expr, $f:path, $sig:literal) => {
        $crate::formula::func::args::def!(@ $name, $min, $max, $f, $sig, false)
    };
    (@ $name:literal, $min:expr, $max:expr, $f:path, $sig:literal, $volatile:expr) => {
        $crate::formula::func::FuncDef {
            name: $name,
            min_args: $min,
            max_args: $max,
            sig: $sig,
            f: |args: &[$crate::formula::func::Arg], ctx: &mut dyn $crate::formula::func::Ctx| {
                $crate::formula::func::IntoValue::into_value($f(args, ctx))
            },
            array: None,
            volatile: $volatile,
        }
    };
}
pub(crate) use def;

/// Fold every value of every argument into `acc`, the way Excel's
/// aggregates read them: a value given directly goes to `direct`, each
/// value inside a range or array to `element` (row-major, reading only the
/// used part of a range, so `A:A` is one pass over the data). Omitted
/// arguments and empty values are skipped. A reference to a missing sheet
/// is one `#REF!` element, as Excel reads a deleted sheet's reference. The
/// first error a callback returns ends the fold.
pub fn fold_args<A>(
    args: &[Arg],
    ctx: &mut dyn Ctx,
    mut acc: A,
    direct: impl Fn(&mut A, &Value) -> Result<(), ErrorKind>,
    element: impl Fn(&mut A, &Value) -> Result<(), ErrorKind>,
) -> Result<A, ErrorKind> {
    for arg in args {
        match arg {
            Arg::Omitted | Arg::Value(Value::Empty) => {}
            Arg::Value(v) => direct(&mut acc, v)?,
            Arg::Array(a) => {
                for v in &a.data {
                    if *v != Value::Empty {
                        element(&mut acc, v)?;
                    }
                }
            }
            Arg::Range(r) => {
                if ctx.extent(r.sheet.as_deref()).is_none() {
                    element(&mut acc, &Value::Error(ErrorKind::Ref))?;
                    continue;
                }
                let Some(r) = ctx.data_part(r) else {
                    continue;
                };
                let mut err = None;
                ctx.for_each(&r, &mut |_, _, v| {
                    if err.is_none() && v != Value::Empty {
                        err = element(&mut acc, &v).err();
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
            }
        }
    }
    Ok(acc)
}

/// Every value of an argument, row-major (a lone value as itself; omitted
/// as nothing). A missing sheet is `#REF!`.
pub fn arg_values(arg: &Arg, ctx: &dyn Ctx) -> Result<Vec<Value>, ErrorKind> {
    match arg {
        Arg::Omitted => Ok(Vec::new()),
        Arg::Value(v) => Ok(vec![v.clone()]),
        _ => area(arg, ctx)?.values(ctx),
    }
}

/// An optional argument that was given: `None` when it's left out, past
/// the end of the list or as an empty slot (`ROUND(2.5,)`).
pub fn given(arg: Option<&Arg>) -> Option<&Arg> {
    arg.filter(|a| !matches!(a, Arg::Omitted))
}

/// An optional numeric argument: its default when left out. (An empty cell
/// passed explicitly is 0, as in Excel.)
pub fn opt_num(arg: Option<&Arg>, ctx: &mut dyn Ctx, default: f64) -> Result<f64, ErrorKind> {
    match given(arg) {
        None => Ok(default),
        Some(a) => super::num(a, ctx),
    }
}

/// A number truncated toward zero (Excel's treatment of count and digit
/// arguments), or `#NUM!` when it can't fit an integer.
pub fn trunc_i64(x: f64) -> Result<i64, ErrorKind> {
    if !x.is_finite() || x.abs() >= 9.0e18 {
        return Err(ErrorKind::Num);
    }
    Ok(x.trunc() as i64)
}

/// An integer argument: the number truncated ([`trunc_i64`]).
pub fn int(arg: &Arg, ctx: &mut dyn Ctx) -> Result<i64, ErrorKind> {
    trunc_i64(super::num(arg, ctx)?)
}

/// A 1-based position (`MID`'s start, `VLOOKUP`'s column): below 1 is
/// `#VALUE!`.
pub fn positive(n: i64) -> Result<usize, ErrorKind> {
    if n < 1 {
        Err(ErrorKind::Value)
    } else {
        Ok(n as usize)
    }
}

/// A 1-based position argument ([`int`], then [`positive`]).
pub fn pos_int(arg: &Arg, ctx: &mut dyn Ctx) -> Result<usize, ErrorKind> {
    positive(int(arg, ctx)?)
}

/// An optional integer argument (truncated).
pub fn opt_int(arg: Option<&Arg>, ctx: &mut dyn Ctx, default: i64) -> Result<i64, ErrorKind> {
    match given(arg) {
        None => Ok(default),
        Some(a) => int(a, ctx),
    }
}

/// An optional count argument (`LEFT`'s `num_chars`): truncated, negative
/// is `#VALUE!`.
pub fn count_arg(arg: Option<&Arg>, ctx: &mut dyn Ctx, default: i64) -> Result<usize, ErrorKind> {
    let n = opt_int(arg, ctx, default)?;
    if n < 0 {
        Err(ErrorKind::Value)
    } else {
        Ok(n as usize)
    }
}

/// An optional boolean argument.
pub fn opt_bool(arg: Option<&Arg>, ctx: &mut dyn Ctx, default: bool) -> Result<bool, ErrorKind> {
    match given(arg) {
        None => Ok(default),
        Some(a) => super::scalar(a, ctx).to_bool(),
    }
}

/// The current year, for dates typed without one.
pub fn current_year(ctx: &dyn Ctx) -> i64 {
    date::serial_to_ymd(ctx.now(), ctx.date1904())
        .map(|(y, _, _)| y)
        .unwrap_or(2000)
}

/// A value as a date serial: numbers as-is, text as a number or as a typed
/// date/time (`"2024-03-01"`, `"3/1/2024 2:30 PM"`), booleans as 1/0, empty
/// as 0.
pub fn to_serial(v: &Value, ctx: &dyn Ctx) -> Result<f64, ErrorKind> {
    match v {
        Value::Text(s) => {
            crate::formula::value::parse_typed_number(s, Some(current_year(ctx)), ctx.date1904())
                .ok_or(ErrorKind::Value)
        }
        v => v.to_number(),
    }
}

/// A date argument: [`to_serial`] of the scalar, `#NUM!` when negative or
/// past 9999-12-31.
pub fn date_arg(arg: &Arg, ctx: &mut dyn Ctx) -> Result<f64, ErrorKind> {
    let v = super::scalar(arg, ctx);
    let s = to_serial(&v, ctx)?;
    if s < 0.0 || s >= date::max_serial(ctx.date1904()) + 1.0 {
        return Err(ErrorKind::Num);
    }
    Ok(s)
}
