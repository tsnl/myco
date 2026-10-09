//! Deliberately small frontmatter parser, not a general YAML parser. Required
//! name/description support plain, single-quoted, JSON-compatible double-quoted,
//! and literal/folded block scalars (|, >, |-, >-, |+, >+). Collections, anchors,
//! tags and multiline quoted scalars are rejected. Other top-level keys are ignored.
//! Plain and quoted scalars permit trailing comments; block indicators do not.

pub(super) fn parse(bytes: &[u8]) -> Result<(String, String, bool), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "metadata is not UTF-8")?;
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next() != Some("---") {
        return Err("missing opening frontmatter delimiter".into());
    }
    let header: Vec<_> = lines.take_while(|line| *line != "---").collect();
    // The caller passes exactly the prefix ending at a closing delimiter.
    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = None;
    let mut i = 0;
    while i < header.len() {
        let line = header[i];
        i += 1;
        if line.trim().is_empty() || line.starts_with('#') || line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            return Err("invalid frontmatter field".into());
        };
        if key.trim() == "disable-model-invocation" {
            if disable_model_invocation.is_some() {
                return Err("duplicate disable-model-invocation field".into());
            }
            let value = raw.split_once(" #").map_or(raw, |(value, _)| value).trim();
            disable_model_invocation = Some(match value {
                "true" => true,
                "false" => false,
                _ => {
                    return Err("disable-model-invocation must be an unquoted true or false".into());
                }
            });
            continue;
        }
        let target = match key.trim() {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        if target.is_some() {
            return Err(format!("duplicate {key} field"));
        }
        let value = if matches!(raw.trim(), "|" | ">" | "|-" | ">-" | "|+" | ">+") {
            let folded = raw.trim().starts_with('>');
            let mut block = Vec::new();
            while i < header.len() && (header[i].is_empty() || header[i].starts_with(' ')) {
                block.push(header[i].trim());
                i += 1;
            }
            block.join(if folded { " " } else { "\n" })
        } else {
            scalar(raw.trim())?
        };
        if value.trim().is_empty() {
            return Err(format!("{key} must be nonempty"));
        }
        if value.len() > 2048 {
            return Err(format!("{key} exceeds 2048 bytes"));
        }
        *target = Some(value.trim().to_owned());
    }
    Ok((
        name.ok_or("missing name")?,
        description.ok_or("missing description")?,
        disable_model_invocation.unwrap_or(false),
    ))
}

fn scalar(raw: &str) -> Result<String, String> {
    if raw.starts_with('#') {
        return Ok(String::new());
    }
    if raw.starts_with('"') {
        let mut values = serde_json::Deserializer::from_str(raw).into_iter::<String>();
        let value = values
            .next()
            .and_then(Result::ok)
            .ok_or("invalid double-quoted scalar (JSON escapes supported)")?;
        scalar_tail(&raw[values.byte_offset()..])?;
        return Ok(value);
    }
    if let Some(quoted) = raw.strip_prefix('\'') {
        let mut chars = quoted.char_indices().peekable();
        let mut out = String::new();
        while let Some((index, c)) = chars.next() {
            if c == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                } else {
                    scalar_tail(&raw[index + 2..])?;
                    return Ok(out);
                }
            }
            out.push(c);
        }
        return Err("unterminated single-quoted scalar".into());
    }
    if raw.starts_with(['[', '{', '&', '*', '!', '|', '>', '`', '@']) || raw.contains(": ") {
        return Err("unsupported YAML scalar form".into());
    }
    Ok(raw
        .split_once(" #")
        .map_or(raw, |(text, _)| text)
        .to_owned())
}

fn scalar_tail(tail: &str) -> Result<(), String> {
    if tail.trim().is_empty()
        || tail.starts_with(char::is_whitespace) && tail.trim_start().starts_with('#')
    {
        Ok(())
    } else {
        Err("unexpected text after quoted scalar".into())
    }
}
