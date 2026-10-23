//! Presentation-only localization. Accounting identifiers and captured user data
//! are never translated. The small bounded cache is independent of database caches.
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashMap, sync::OnceLock};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "zh-Hant")]
    Traditional,
    #[serde(rename = "zh-Hans")]
    Simplified,
    #[serde(rename = "en")]
    English,
}
impl Language {
    pub const ALL: [Self; 3] = [Self::Traditional, Self::Simplified, Self::English];
    pub fn label(self) -> &'static str {
        match self {
            Self::Traditional => "繁體中文",
            Self::Simplified => "简体中文",
            Self::English => "English",
        }
    }
}
thread_local! {
    static STATE: RefCell<(Language, HashMap<String, String>)> = RefCell::new((Language::Traditional, HashMap::new()));
}
pub fn set_language(language: Language) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.0 != language {
            state.0 = language;
            state.1.clear();
        }
    });
}

#[derive(Debug)]
struct Template {
    parts: Vec<String>,
    keys: Vec<String>,
}
impl Template {
    fn parse(text: &str) -> Self {
        let mut parts = Vec::new();
        let mut keys = Vec::new();
        let mut rest = text;
        let mut index = 0;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}').map(|i| open + i) else {
                break;
            };
            parts.push(rest[..open].into());
            let key = rest[open + 1..close].split(':').next().unwrap();
            keys.push(if key.is_empty() {
                let value = index.to_string();
                index += 1;
                value
            } else {
                key.into()
            });
            rest = &rest[close + 1..];
        }
        parts.push(rest.into());
        Self { parts, keys }
    }
    fn capture<'a>(&self, text: &'a str) -> Option<Vec<&'a str>> {
        let mut rest = text.strip_prefix(&self.parts[0])?;
        let mut values = Vec::new();
        for (index, next) in self.parts.iter().skip(1).enumerate() {
            let end = if index + 1 == self.keys.len() {
                rest.len()
                    .checked_sub(next.len())
                    .filter(|_| rest.ends_with(next))?
            } else {
                rest.find(next)?
            };
            values.push(&rest[..end]);
            rest = &rest[end + next.len()..];
        }
        rest.is_empty().then_some(values)
    }
    fn render(&self, source: &Self, values: &[&str]) -> String {
        let mut result = self.parts[0].clone();
        for (index, key) in self.keys.iter().enumerate() {
            if let Some(source_index) = source.keys.iter().position(|k| k == key) {
                result.push_str(values[source_index]);
            }
            result.push_str(&self.parts[index + 1]);
        }
        result
    }
}
struct Entry {
    source: String,
    original: Template,
    english: Template,
    simplified: Template,
}
fn catalog() -> &'static Vec<Entry> {
    static CATALOG: OnceLock<Vec<Entry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut entries = Vec::new();
        let mut add = |source: &str, english: &str, simplified: &str| {
            if !entries.iter().any(|entry: &Entry| entry.source == source) {
                entries.push(Entry {
                    source: source.into(),
                    original: Template::parse(source),
                    english: Template::parse(english),
                    simplified: Template::parse(simplified),
                });
            }
        };
        for line in include_str!("../assets/translations.tsv")
            .lines()
            .filter(|line| !line.is_empty())
        {
            let columns: Vec<_> = line
                .split('\t')
                .map(|value| value.replace("\\n", "\n"))
                .collect();
            let [source, english, simplified] = columns.as_slice() else {
                panic!("invalid translation row");
            };
            add(source, english, simplified);
            // Tooltips concatenate lines; derive their translations from the
            // same catalog, preserving names and paths inside each template.
            if source.contains('\n')
                && source.lines().count() == english.lines().count()
                && source.lines().count() == simplified.lines().count()
            {
                for ((source, english), simplified) in
                    source.lines().zip(english.lines()).zip(simplified.lines())
                {
                    if !source.is_empty() {
                        add(source, english, simplified);
                    }
                }
            }
        }
        entries.sort_by_key(|e| {
            std::cmp::Reverse(e.original.parts.iter().map(String::len).sum::<usize>())
        });
        entries
    })
}
pub fn translate(language: Language, text: &str) -> String {
    if language == Language::Traditional
        || !text.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
    {
        return text.into();
    }
    // Exact matches precede formatted messages to avoid a broad placeholder
    // swallowing a more specific label. Captures retain names, paths and IDs.
    for entry in catalog() {
        if entry.source == text {
            let target = if language == Language::English {
                &entry.english
            } else {
                &entry.simplified
            };
            return target.parts.join("");
        }
    }
    for entry in catalog().iter().filter(|e| !e.original.keys.is_empty()) {
        if let Some(values) = entry.original.capture(text) {
            return if language == Language::English {
                &entry.english
            } else {
                &entry.simplified
            }
            .render(&entry.original, &values);
        }
    }
    if text.contains('\n') {
        return text
            .split('\n')
            .map(|line| translate(language, line))
            .collect::<Vec<_>>()
            .join("\n");
    }
    text.into()
}
pub fn tr(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.0 == Language::Traditional {
            return text.to_owned();
        }
        if let Some(value) = state.1.get(text) {
            return value.clone();
        }
        let value = translate(state.0, text);
        if state.1.len() >= 2048 {
            state.1.clear();
        }
        state.1.insert(text.into(), value.clone());
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_placeholders_and_language_persistence() {
        for entry in catalog() {
            let mut original = entry.original.keys.clone();
            original.sort();
            let mut english = entry.english.keys.clone();
            english.sort();
            assert_eq!(original, english, "{}", entry.source);
            let mut simplified = entry.simplified.keys.clone();
            simplified.sort();
            assert_eq!(original, simplified, "{}", entry.source);
        }
        for language in Language::ALL {
            let mut settings = crate::storage::Settings::default();
            settings.language = language;
            let saved = serde_json::to_string(&settings).unwrap();
            assert_eq!(
                serde_json::from_str::<crate::storage::Settings>(&saved)
                    .unwrap()
                    .language,
                language
            );
        }
    }
    #[test]
    fn switching_preserves_user_data_and_formats() {
        assert_eq!(
            translate(Language::English, "重設 2d 01:02:03"),
            "Reset 2d 01:02:03"
        );
        assert_eq!(
            translate(Language::English, "Quota Widget · 扣額紀錄"),
            "Quota Widget · Quota history"
        );
        assert_eq!(
            translate(Language::English, "已匯出：N:\\私人\\文件.json"),
            "Exported: N:\\私人\\文件.json"
        );
        assert_eq!(translate(Language::English, "刀劍封魔錄"), "刀劍封魔錄");
        assert_eq!(
            translate(Language::English, "169 筆 Response\n$13.2912"),
            "169 responses\n$13.2912"
        );
        set_language(Language::English);
        assert_eq!(tr("對話"), "Conversation");
        set_language(Language::Traditional);
        assert_eq!(tr("對話"), "對話");
    }
    #[test]
    fn simplified_chinese_preserves_paths() {
        assert_eq!(translate(Language::Simplified, "扣額紀錄"), "扣额记录");
        assert_eq!(
            translate(Language::Simplified, "已匯出：N:\\私人\\文件.json"),
            "已导出：N:\\私人\\文件.json"
        );
    }
}
