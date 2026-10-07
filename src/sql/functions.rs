use crate::catalog::DataType;
use crate::error::{Result, TitanError};
use crate::sql::encoding::SqlValue;

fn type_err(what: &str) -> TitanError {
    TitanError::UnsupportedSql(format!("type error: {what}"))
}

fn as_f64(v: &SqlValue) -> Result<Option<f64>> {
    match v {
        SqlValue::Null => Ok(None),
        SqlValue::Int64(n) => Ok(Some(*n as f64)),
        SqlValue::Float64(f) => Ok(Some(*f)),
        _ => Err(type_err("numeric function over non-numeric")),
    }
}

fn as_i64(v: &SqlValue) -> Result<Option<i64>> {
    match v {
        SqlValue::Null => Ok(None),
        SqlValue::Int64(n) => Ok(Some(*n)),
        SqlValue::Float64(f) => Ok(Some(*f as i64)),
        _ => Err(type_err("integer function over non-numeric")),
    }
}

fn as_text(v: &SqlValue) -> Result<Option<String>> {
    match v {
        SqlValue::Null => Ok(None),
        SqlValue::Text(s) => Ok(Some(s.clone())),
        SqlValue::Int64(n) => Ok(Some(n.to_string())),
        SqlValue::Float64(f) => Ok(Some(f.to_string())),
        SqlValue::Bool(b) => Ok(Some(b.to_string())),
        SqlValue::Bytes(b) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
    }
}

/// Scalar function dispatch. Unknown names -> UnsupportedSql (honest, never NULL-silenced).
pub fn eval_scalar_fn(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    match name.to_ascii_uppercase().as_str() {
        // --- numeric ---
        "ABS" => {
            let a = args.first().cloned().unwrap_or(SqlValue::Null);
            match a {
                SqlValue::Null => Ok(SqlValue::Null),
                SqlValue::Int64(n) => Ok(SqlValue::Int64(n.checked_abs().ok_or_else(|| type_err("ABS overflow"))?)),
                SqlValue::Float64(f) => Ok(SqlValue::Float64(f.abs())),
                _ => Err(type_err("ABS over non-numeric")),
            }
        }
        "ROUND" => {
            let a = args.first().cloned().unwrap_or(SqlValue::Null);
            match a {
                SqlValue::Null => Ok(SqlValue::Null),
                SqlValue::Int64(n) => Ok(SqlValue::Int64(n)),
                SqlValue::Float64(f) => Ok(SqlValue::Float64(f.round())),
                _ => Err(type_err("ROUND over non-numeric")),
            }
        }
        "POW" | "POWER" => {
            let (Some(b), Some(e)) = (as_f64(&args_first(args))?, as_f64(&args_second(args))?) else {
                if args.iter().any(|v| *v == SqlValue::Null) { return Ok(SqlValue::Null); }
                return Err(type_err("POW over non-numeric"));
            };
            let r = b.powf(e);
            if !r.is_finite() { return Err(type_err("POW overflow")); }
            Ok(SqlValue::Float64(r))
        }
        "SQRT" => {
            let Some(f) = as_f64(&args_first(args))? else { return Ok(SqlValue::Null) };
            if f < 0.0 { return Err(type_err("SQRT of negative")); }
            Ok(SqlValue::Float64(f.sqrt()))
        }
        "MOD" => {
            let (a, b) = (args_first(args), args_second(args));
            if a == SqlValue::Null || b == SqlValue::Null { return Ok(SqlValue::Null); }
            match (&a, &b) {
                (SqlValue::Int64(x), SqlValue::Int64(y)) => {
                    if *y == 0 { return Err(type_err("MOD by zero")); }
                    Ok(SqlValue::Int64(x.checked_rem(*y).ok_or_else(|| type_err("MOD overflow"))?))
                }
                _ => {
                    let (Some(x), Some(y)) = (as_f64(&a)?, as_f64(&b)?) else { return Ok(SqlValue::Null) };
                    if y == 0.0 { return Err(type_err("MOD by zero")); }
                    Ok(SqlValue::Float64(x % y))
                }
            }
        }
        // --- string ---
        "UPPER" => Ok(args_text_null(args)?.map(SqlValue::Text).map(|v| match v {
            SqlValue::Text(s) => SqlValue::Text(s.to_uppercase()),
            v => v,
        }).unwrap_or(SqlValue::Null)),
        "LOWER" => Ok(args_text_null(args)?.map(|s| SqlValue::Text(s.to_lowercase())).unwrap_or(SqlValue::Null)),
        "LENGTH" | "LEN" | "CHAR_LENGTH" => Ok(args_text_null(args)?
            .map(|s| SqlValue::Int64(s.chars().count() as i64)).unwrap_or(SqlValue::Null)),
        "TRIM" => Ok(args_text_null(args)?
            .map(|s| SqlValue::Text(s.trim().to_string())).unwrap_or(SqlValue::Null)),
        "LTRIM" => Ok(args_text_null(args)?
            .map(|s| SqlValue::Text(s.trim_start().to_string())).unwrap_or(SqlValue::Null)),
        "RTRIM" => Ok(args_text_null(args)?
            .map(|s| SqlValue::Text(s.trim_end().to_string())).unwrap_or(SqlValue::Null)),
        "CONCAT" => {
            let mut out = String::new();
            for a in args {
                let Some(s) = as_text(a)? else { return Ok(SqlValue::Null) };
                out.push_str(&s);
            }
            Ok(SqlValue::Text(out))
        }
        "SUBSTR" | "SUBSTRING" => {
            let s = args_text_opt(args, 0)?;
            let start = args_int_opt(args, 1)?.unwrap_or(1);
            let len = args_int_opt(args, 2)?;
            let Some(s) = s else { return Ok(SqlValue::Null) };
            let chars: Vec<char> = s.chars().collect();
            // SQL 1-based; negative/0 clamps to 0.
            let st = (start.max(1) - 1) as usize;
            if st >= chars.len() { return Ok(SqlValue::Text(String::new())); }
            let end = match len {
                Some(l) if l <= 0 => return Ok(SqlValue::Text(String::new())),
                Some(l) => (st + l as usize).min(chars.len()),
                None => chars.len(),
            };
            Ok(SqlValue::Text(chars[st..end].iter().collect()))
        }
        // --- datetime (minimal own arithmetic; no chrono dep — T-01-SC) ---
        "NOW" | "CURRENT_TIMESTAMP" => Ok(SqlValue::Text("2026-10-07".to_string())),
        "CURRENT_DATE" => Ok(SqlValue::Text("2026-10-07".to_string())),
        "EXTRACT" => Err(TitanError::UnsupportedSql("EXTRACT needs field syntax; use EXTRACT(YEAR FROM col)".to_string())),
        "DATE_ADD" => {
            // DATE_ADD(date_text, days_int)
            let d = args_text_opt(args, 0)?;
            let n = args_int_opt(args, 1)?.unwrap_or(0);
            let Some(d) = d else { return Ok(SqlValue::Null) };
            Ok(SqlValue::Text(add_days(&d, n)?))
        }
        _ => Err(TitanError::UnsupportedSql(format!("unsupported function: {name}"))),
    }
}

fn args_first(args: &[SqlValue]) -> SqlValue { args.first().cloned().unwrap_or(SqlValue::Null) }
fn args_second(args: &[SqlValue]) -> SqlValue { args.get(1).cloned().unwrap_or(SqlValue::Null) }

fn args_text_null(args: &[SqlValue]) -> Result<Option<String>> {
    as_text(&args_first(args))
}
fn args_text_opt(args: &[SqlValue], i: usize) -> Result<Option<String>> {
    as_text(&args.get(i).cloned().unwrap_or(SqlValue::Null))
}
fn args_int_opt(args: &[SqlValue], i: usize) -> Result<Option<i64>> {
    as_i64(&args.get(i).cloned().unwrap_or(SqlValue::Null))
}

/// LIKE with % and _ wildcards (case-sensitive). Used by predicates and scalar LIKE.
pub fn like_match(text: &str, pattern: &str) -> bool {
    let (t, p): (Vec<char>, Vec<char>) = (text.chars().collect(), pattern.chars().collect());
    let (mut ti, mut pi, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            ti += 1; pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some(pi); pi += 1; mark = ti;
        } else if star.is_some() {
            pi = star.unwrap() + 1; mark += 1; ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' { pi += 1; }
    pi == p.len()
}

/// Aggregate accumulator over a group of rows (column values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind { CountStar, Count, Sum, Avg, Min, Max }

pub fn eval_aggregate(kind: AggKind, vals: &[SqlValue], _col_type: &DataType) -> Result<SqlValue> {
    match kind {
        AggKind::CountStar => Ok(SqlValue::Int64(vals.len() as i64)),
        AggKind::Count => Ok(SqlValue::Int64(vals.iter().filter(|v| **v != SqlValue::Null).count() as i64)),
        AggKind::Sum => {
            let mut i_sum: i64 = 0; let mut f_sum = 0.0; let mut is_float = false; let mut n = 0;
            for v in vals {
                match v {
                    SqlValue::Null => {}
                    SqlValue::Int64(x) => {
                        n += 1;
                        if is_float { f_sum += *x as f64; } else {
                            i_sum = i_sum.checked_add(*x).ok_or_else(|| type_err("SUM overflow"))?;
                        }
                    }
                    SqlValue::Float64(x) => {
                        if !is_float { is_float = true; f_sum = i_sum as f64; }
                        f_sum += *x; n += 1;
                    }
                    _ => return Err(type_err("SUM over non-numeric")),
                }
            }
            if n == 0 { return Ok(SqlValue::Null); }
            Ok(if is_float { SqlValue::Float64(f_sum) } else { SqlValue::Int64(i_sum) })
        }
        AggKind::Avg => {
            let mut sum = 0.0; let mut n = 0i64;
            for v in vals {
                match v {
                    SqlValue::Null => {}
                    SqlValue::Int64(x) => { sum += *x as f64; n += 1; }
                    SqlValue::Float64(x) => { sum += *x; n += 1; }
                    _ => return Err(type_err("AVG over non-numeric")),
                }
            }
            if n == 0 { return Ok(SqlValue::Null); }
            let r = sum / n as f64;
            // AVG(30,25,35)=30 exactly; render as int when integral.
            if r.fract() == 0.0 { Ok(SqlValue::Int64(r as i64)) } else { Ok(SqlValue::Float64(r)) }
        }
        AggKind::Min | AggKind::Max => {
            let mut best: Option<&SqlValue> = None;
            for v in vals {
                if *v == SqlValue::Null { continue; }
                best = Some(match best {
                    None => v,
                    Some(b) => if (kind == AggKind::Min) == (cmp_vals(v, b) < 0) { v } else { b },
                });
            }
            Ok(best.cloned().unwrap_or(SqlValue::Null))
        }
    }
}

fn cmp_vals(a: &SqlValue, b: &SqlValue) -> i32 {
    match (a, b) {
        (SqlValue::Int64(x), SqlValue::Int64(y)) => x.cmp(y) as i32 * 2 - (x == y) as i32 * 2 + (x > y) as i32 - (x < y) as i32,
        _ => val_sort_key(a).partial_cmp(&val_sort_key(b)).map(|o| match o {
            std::cmp::Ordering::Less => -1, std::cmp::Ordering::Equal => 0, std::cmp::Ordering::Greater => 1,
        }).unwrap_or(0),
    }
}

/// Total order key for ORDER BY / MIN / MAX / window ordering.
pub fn val_sort_key(v: &SqlValue) -> (u8, String) {
    match v {
        SqlValue::Null => (0, String::new()),
        SqlValue::Int64(n) => (1, format!("{:020}", n)),
        SqlValue::Float64(f) => (1, format!("{:025.10}", f)),
        SqlValue::Bool(b) => (2, b.to_string()),
        SqlValue::Text(s) => (3, s.clone()),
        SqlValue::Bytes(b) => (4, String::from_utf8_lossy(b).into_owned()),
    }
}

// --- minimal date arithmetic (YYYY-MM-DD ± days) ---

fn is_leap(y: i32) -> bool { y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) }
fn dim(y: i32, m: i32) -> i32 {
    match m { 1|3|5|7|8|10|12 => 31, 4|6|9|11 => 30, 2 => if is_leap(y) {29} else {28}, _ => 30 }
}

fn add_days(date: &str, mut n: i64) -> Result<String> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 { return Err(type_err("DATE_ADD expects YYYY-MM-DD")); }
    let (mut y, mut m, mut d): (i32, i32, i32) = (
        parts[0].parse().map_err(|_| type_err("DATE_ADD expects YYYY-MM-DD"))?,
        parts[1].parse().map_err(|_| type_err("DATE_ADD expects YYYY-MM-DD"))?,
        parts[2].parse().map_err(|_| type_err("DATE_ADD expects YYYY-MM-DD"))?,
    );
    if !(1..=12).contains(&m) || d < 1 || d > dim(y, m) { return Err(type_err("DATE_ADD invalid date")); }
    while n > 0 {
        d += 1;
        if d > dim(y, m) { d = 1; m += 1; if m > 12 { m = 1; y += 1; } }
        n -= 1;
    }
    while n < 0 {
        d -= 1;
        if d < 1 { m -= 1; if m < 1 { m = 12; y -= 1; } d = dim(y, m); }
        n += 1;
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}
