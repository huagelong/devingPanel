use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

impl Language {
    pub fn from_saved(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("en") => Self::English,
            _ => Self::Chinese,
        }
    }

    pub fn saved(self) -> &'static str {
        match self {
            Self::Chinese => "zh-CN",
            Self::English => "en",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Chinese => "简体中文",
            Self::English => "English",
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);
static ENGLISH: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn set_current(language: Language) {
    CURRENT.store(u8::from(language == Language::English), Ordering::Relaxed);
}

pub fn current() -> Language {
    if CURRENT.load(Ordering::Relaxed) == 1 {
        Language::English
    } else {
        Language::Chinese
    }
}

fn english(source: &str) -> Option<&'static str> {
    ENGLISH
        .get_or_init(|| {
            serde_json::from_str(include_str!("i18n_en.json")).expect("valid English messages")
        })
        .get(source)
        .map(String::as_str)
}

pub fn tr(source: &str) -> String {
    tr_in(current(), source)
}

fn tr_in(language: Language, source: &str) -> String {
    if language == Language::English {
        english(source).unwrap_or(source).to_owned()
    } else {
        source.to_owned()
    }
}

pub fn trf(template: &str, rendered: String) -> String {
    trf_in(current(), template, rendered)
}

fn trf_in(language: Language, template: &str, rendered: String) -> String {
    if language == Language::Chinese {
        return rendered;
    }
    let Some(translated) = english(template) else {
        return rendered;
    };
    render_template(template, translated, &rendered).unwrap_or(rendered)
}

pub fn tr_system(message: &str) -> String {
    if current() == Language::Chinese {
        return message.to_owned();
    }
    if let Some(translation) = english(message) {
        return translation.to_owned();
    }
    let mut best: Option<(usize, String)> = None;
    for (template, translation) in ENGLISH.get().expect("English messages loaded") {
        if !template.contains('{') {
            continue;
        }
        if let Some(result) = render_template(template, translation, message) {
            let score = template.len();
            if best.as_ref().is_none_or(|(old, _)| score > *old) {
                best = Some((score, result));
            }
        }
    }
    best.map(|(_, result)| result)
        .unwrap_or_else(|| message.to_owned())
}

fn render_template(template: &str, translated: &str, rendered: &str) -> Option<String> {
    let (parts, slots) = split_template(template)?;
    if slots == 0 {
        return (template == rendered).then(|| translated.to_owned());
    }
    let mut values = Vec::with_capacity(slots);
    let mut rest = rendered;
    if !rest.starts_with(&parts[0]) {
        return None;
    }
    rest = &rest[parts[0].len()..];
    for part in parts.iter().skip(1) {
        if part.is_empty() {
            if values.len() + 1 != slots {
                return None;
            }
            values.push(rest);
            rest = "";
        } else if let Some(index) = rest.find(part) {
            values.push(&rest[..index]);
            rest = &rest[index + part.len()..];
        } else {
            return None;
        }
    }
    if !rest.is_empty() || values.len() != slots {
        return None;
    }
    let (english_parts, english_slots) = split_template(translated)?;
    if english_slots != slots {
        return None;
    }
    let mut result = english_parts[0].clone();
    for (value, part) in values.iter().zip(english_parts.iter().skip(1)) {
        result.push_str(value);
        result.push_str(part);
    }
    Some(result)
}

fn split_template(template: &str) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![String::new()];
    let mut slots = 0;
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                parts.last_mut()?.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                parts.last_mut()?.push('}');
            }
            '{' => {
                while chars.next()? != '}' {}
                slots += 1;
                parts.push(String::new());
            }
            '}' => return None,
            _ => parts.last_mut()?.push(ch),
        }
    }
    Some((parts, slots))
}

#[cfg(test)]
mod tests {
    use super::{Language, tr_in, trf_in};

    #[test]
    fn language_defaults_to_chinese_and_translates_interpolation_without_changing_values() {
        assert_eq!(
            Language::from_saved(Some("invalid".into())),
            Language::Chinese
        );
        assert_eq!(tr_in(Language::English, "设置"), "Settings");
        assert_eq!(
            trf_in(
                Language::English,
                "目标主机：{name} · Debian 13 / amd64 · root",
                "目标主机：用户主机 · Debian 13 / amd64 · root".into()
            ),
            "Target host: 用户主机 · Debian 13/amd64 · root"
        );
    }
}
