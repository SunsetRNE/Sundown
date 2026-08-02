//! 极简 TOML 子集解析器（L3 策略文件，零依赖——与 JSON 手写同哲学）。
//!
//! 支持的子集（docs/l3-plan.md §0.2）：
//!   - 表头 `[a.b]`（嵌套路径，点分隔）
//!   - `key = value`：字符串（双引号，`\"` `\\` `\n` `\t` 转义）、布尔、整数、
//!     字符串数组 `[ "a", "b" ]`
//!   - `#` 行注释与行尾注释
//!   - 顶层键（无表头）归入空路径
//!
//! 不做：多行字符串、内联表、日期、浮点（策略面用不到）。
//! 错误处理：返回 Err(行号+描述)，由 policy.rs 保留旧策略表。

/// 解析出的值类型
#[derive(Debug, Clone, PartialEq)]
pub enum TomlValue {
    Str(String),
    Bool(bool),
    Int(i64),
    StrArray(Vec<String>),
}

/// 一条键值：表路径 + 键 + 值
#[derive(Debug, Clone)]
pub struct TomlEntry {
    pub table: Vec<String>,
    pub key: String,
    pub value: TomlValue,
}

/// 解析整个文本；失败返回 Err(行号, 描述)
pub fn parse(src: &str) -> Result<Vec<TomlEntry>, (usize, String)> {
    let mut out: Vec<TomlEntry> = Vec::new();
    let mut table: Vec<String> = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let lineno = i + 1;
        let line = strip_comment(lines[i]).trim();
        if line.is_empty() {
            i += 1;
            continue;
        }
        if line.starts_with('[') {
            // 表头 [a.b]
            if !line.ends_with(']') {
                return Err((lineno, format!("表头未闭合: {}", line)));
            }
            let inner = &line[1..line.len() - 1];
            // 表头段拆分：引号段优先（包名含点，如 [apps."com.tencent.mm"]），
            // 引号段整体作为一段（引号内不做转义——包名无转义需求），
            // 其余按 '.' 切分裸段。
            table = split_table_header(inner);
            i += 1;
            continue;
        }
        // key = value
        let eq = match line.find('=') {
            Some(p) => p,
            None => return Err((lineno, format!("缺 '=': {}", line))),
        };
        let key = line[..eq].trim().to_string();
        if key.is_empty() {
            return Err((lineno, format!("空键名: {}", line)));
        }
        let mut val_str = line[eq + 1..].trim().to_string();
        // 多行数组：值以 [ 开头但未闭合 → 拼接后续行（去注释/空白）直到 ]
        if val_str.starts_with('[') && !val_str.ends_with(']') {
            let mut closed = false;
            while i + 1 < lines.len() {
                i += 1;
                let l2 = strip_comment(lines[i]).trim();
                if l2.is_empty() {
                    continue;
                }
                if l2.contains(']') {
                    closed = true;
                }
                val_str.push_str(l2);
                if closed {
                    break;
                }
            }
            if !val_str.ends_with(']') {
                return Err((lineno, format!("数组未闭合: {}", val_str)));
            }
        }
        let value = parse_value(&val_str, lineno)?;
        out.push(TomlEntry {
            table: table.clone(),
            key,
            value,
        });
        i += 1;
    }
    Ok(out)
}

/// 表头段拆分：[apps."com.tencent.mm"] → ["apps", "com.tencent.mm"]
/// 引号段整体为一段（含点）；裸段按 '.' 切分；未闭合引号按裸段宽容处理
fn split_table_header(inner: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = inner;
    loop {
        match rest.find('"') {
            Some(q) => {
                // 引号前的裸段
                for seg in rest[..q]
                    .split('.')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    out.push(seg.to_string());
                }
                match rest[q + 1..].find('"') {
                    Some(e) => {
                        out.push(rest[q + 1..q + 1 + e].to_string());
                        rest = &rest[q + 1 + e + 1..];
                    }
                    None => {
                        // 未闭合引号：余下全部按裸段处理（宽容）
                        for seg in rest[q..]
                            .split('.')
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                        {
                            out.push(seg.to_string());
                        }
                        break;
                    }
                }
            }
            None => {
                for seg in rest
                    .split('.')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    out.push(seg.to_string());
                }
                break;
            }
        }
    }
    out
}

fn strip_comment(line: &str) -> &str {
    // 字符串外的 # 视为注释；简化实现：不在双引号内即截断（策略文件字符串不含 #）
    match line.find('#') {
        Some(p) => {
            // 粗略校验 # 是否在引号内：数前面的引号个数
            let before = &line[..p];
            let quotes = before.bytes().filter(|b| *b == b'"').count();
            if quotes % 2 == 0 {
                &line[..p]
            } else {
                line
            }
        }
        None => line,
    }
}

fn parse_value(s: &str, lineno: usize) -> Result<TomlValue, (usize, String)> {
    if s.is_empty() {
        return Err((lineno, "空值".to_string()));
    }
    // 字符串数组 [ "a", "b" ]
    if s.starts_with('[') {
        if !s.ends_with(']') {
            return Err((lineno, format!("数组未闭合: {}", s)));
        }
        let inner = s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return Ok(TomlValue::StrArray(Vec::new()));
        }
        let mut items = Vec::new();
        // 按逗号分割（引号内逗号不分割——策略场景包名无逗号，简化处理）；
        // 容忍空元素（多行数组的尾逗号 `"a",` 常见）
        for part in inner.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if p.len() < 2 || !p.starts_with('"') || !p.ends_with('"') {
                return Err((lineno, format!("数组元素须为双引号字符串: {}", p)));
            }
            items.push(unescape(&p[1..p.len() - 1], lineno)?);
        }
        return Ok(TomlValue::StrArray(items));
    }
    // 字符串
    if s.starts_with('"') {
        if !s.ends_with('"') || s.len() < 2 {
            return Err((lineno, format!("字符串未闭合: {}", s)));
        }
        return Ok(TomlValue::Str(unescape(&s[1..s.len() - 1], lineno)?));
    }
    // 布尔
    if s == "true" || s == "false" {
        return Ok(TomlValue::Bool(s == "true"));
    }
    // 整数
    if let Ok(n) = s.parse::<i64>() {
        return Ok(TomlValue::Int(n));
    }
    Err((lineno, format!("不支持的值类型: {}", s)))
}

fn unescape(s: &str, lineno: usize) -> Result<String, (usize, String)> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    return Err((
                        lineno,
                        format!("不支持的转义: \\{}", other),
                    ))
                }
                None => return Err((lineno, "结尾反斜杠".to_string())),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let src = r#"
# 注释
[general]
enabled = true
grace_seconds = 10

[whitelist]
packages = [ "com.android.settings", "com.android.systemui" ]
"#;
        let entries = parse(src).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].table, vec!["general"]);
        assert_eq!(entries[0].key, "enabled");
        assert_eq!(entries[0].value, TomlValue::Bool(true));
        assert_eq!(entries[1].value, TomlValue::Int(10));
        match &entries[2].value {
            TomlValue::StrArray(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0], "com.android.settings");
            }
            _ => panic!("期望数组"),
        }
    }

    #[test]
    fn parse_multiline_array() {
        let src = r#"
[whitelist]
packages = [
    "com.android.systemui",
    # 行内注释
    "com.oplus.launcher",
    "com.google.android.gms",
]
keep_fg_service = true
"#;
        let entries = parse(src).unwrap();
        assert_eq!(entries.len(), 2);
        match &entries[0].value {
            TomlValue::StrArray(v) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0], "com.android.systemui");
                assert_eq!(v[1], "com.oplus.launcher");
                assert_eq!(v[2], "com.google.android.gms");
            }
            _ => panic!("期望数组"),
        }
        assert_eq!(entries[1].value, TomlValue::Bool(true));
        // 未闭合数组 → Err
        assert!(parse("packages = [\n  \"a\"\n").is_err());
    }

    #[test]
    fn parse_escape_and_error() {
        assert_eq!(
            parse("k = \"a\\\"b\"").unwrap()[0].value,
            TomlValue::Str("a\"b".to_string())
        );
        assert!(parse("k = ").is_err());
        assert!(parse("[a\n").is_err());
        assert!(parse("k = 3.14").is_err());
    }

    #[test]
    fn parse_quoted_section() {
        // per-app 策略表头：[apps."com.tencent.mm"]（包名含点，引号段）
        let src = r#"
[apps."com.tencent.mm"]
mode = "strict"
grace_seconds = 8
"#;
        let entries = parse(src).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].table, vec!["apps", "com.tencent.mm"]);
        assert_eq!(entries[0].key, "mode");
        assert_eq!(entries[0].value, TomlValue::Str("strict".to_string()));
        assert_eq!(entries[1].table, vec!["apps", "com.tencent.mm"]);
        assert_eq!(entries[1].value, TomlValue::Int(8));
        // 普通表头不受影响
        let e2 = parse("[general]\nenabled = true").unwrap();
        assert_eq!(e2[0].table, vec!["general"]);
    }
}