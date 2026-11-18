use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use regex::Regex;
use std::{ops::Range, path::Path, sync::LazyLock};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

const NAMES: &[&str] = &[
    "keyword",
    "type",
    "function",
    "method",
    "constructor",
    "string",
    "character",
    "number",
    "float",
    "constant",
    "boolean",
    "comment",
    "tag",
    "attribute",
    "property",
    "operator",
    "label",
    "escape",
];

fn color(name: &str) -> Style {
    let fg = match name {
        "keyword" => Color::Rgb(197, 155, 239),
        "type" | "constructor" | "tag" => Color::Rgb(113, 206, 191),
        "function" | "method" | "label" => Color::Rgb(125, 179, 242),
        "string" | "character" => Color::Rgb(157, 202, 133),
        "number" | "float" | "constant" | "boolean" | "escape" => Color::Rgb(235, 180, 121),
        "comment" => {
            return Style::new()
                .fg(Color::Rgb(136, 149, 165))
                .add_modifier(Modifier::ITALIC);
        }
        "attribute" | "property" => Color::Rgb(233, 202, 133),
        _ => Color::Rgb(169, 181, 204),
    };
    Style::new().fg(fg)
}

fn config(
    language: tree_sitter::Language,
    name: &str,
    query: &str,
) -> Option<HighlightConfiguration> {
    let mut config = HighlightConfiguration::new(language, name, query, "", "").ok()?;
    config.configure(NAMES);
    Some(config)
}

fn grammar(extension: &str) -> Option<&'static HighlightConfiguration> {
    static JAVA: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_java::LANGUAGE.into(),
            "Java",
            tree_sitter_java::HIGHLIGHTS_QUERY,
        )
    });
    static PY: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_python::LANGUAGE.into(),
            "Python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
        )
    });
    static JS: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_javascript::LANGUAGE.into(),
            "JavaScript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        )
    });
    static TS: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "TypeScript",
            &format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY
            ),
        )
    });
    static TSX: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "TSX",
            &format!(
                "{}\n{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            ),
        )
    });
    static RS: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_rust::LANGUAGE.into(),
            "Rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        )
    });
    static YAML: LazyLock<Option<HighlightConfiguration>> = LazyLock::new(|| {
        config(
            tree_sitter_yaml::LANGUAGE.into(),
            "YAML",
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
        )
    });
    match extension {
        "java" => JAVA.as_ref(),
        "py" | "pyi" => PY.as_ref(),
        "js" | "mjs" | "cjs" | "jsx" => JS.as_ref(),
        "ts" | "mts" | "cts" => TS.as_ref(),
        "tsx" => TSX.as_ref(),
        "rs" => RS.as_ref(),
        "yaml" | "yml" => YAML.as_ref(),
        _ => None,
    }
}

static XML: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<!--.*?-->|<!\[CDATA\[.*?\]\]>|</?[\w:.-]+(?:[^<>"']|"[^"]*"|'[^']*')*>|<\?.*?\?>"#,
    )
    .unwrap()
});
static XML_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?P<tag>^</?\??[\w:.-]+)|(?P<string>"[^"]*"|'[^']*')|(?P<attribute>[\w:.-]+)\s*="#,
    )
    .unwrap()
});
static MARKDOWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?m)^[ \t]{0,3}#{1,6}[ \t]+[^\n]*|^[ \t]*(?:```|~~~)[^\n]*|\[[^\]\n]+\]\([^\n)]*\)|`[^`\n]+`|\*\*[^*\n]+\*\*",
).unwrap()
});

#[derive(Default)]
pub struct Syntax {
    starts: Vec<usize>,
    ranges: Vec<(Range<usize>, Style)>,
}

impl Syntax {
    pub fn parse(path: &str, body: Option<&str>) -> Self {
        let Some(body) = body else {
            return Self::default();
        };
        let ext = Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut ranges = Vec::new();
        if let Some(config) = grammar(&ext) {
            let mut highlighter = Highlighter::new();
            if let Ok(events) = highlighter.highlight(config, body.as_bytes(), None, |_| None) {
                let mut styles = Vec::new();
                for event in events {
                    match event {
                        Ok(HighlightEvent::HighlightStart(h)) => styles.push(color(NAMES[h.0])),
                        Ok(HighlightEvent::HighlightEnd) => {
                            styles.pop();
                        }
                        Ok(HighlightEvent::Source { start, end }) if start < end => {
                            if let Some(&style) = styles.last() {
                                ranges.push((start..end, style));
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        } else if ext == "xml" || ext == "xsd" || ext == "svg" {
            for token in XML.find_iter(body) {
                if token.as_str().starts_with("<!") {
                    ranges.push((token.range(), color("comment")));
                    continue;
                }
                for capture in XML_TOKEN.captures_iter(token.as_str()) {
                    for name in ["tag", "string", "attribute"] {
                        if let Some(part) = capture.name(name) {
                            ranges.push((
                                token.start() + part.start()..token.start() + part.end(),
                                color(name),
                            ));
                        }
                    }
                }
            }
        } else if ext == "md" || ext == "markdown" {
            for token in MARKDOWN.find_iter(body) {
                let raw = token.as_str().trim_start();
                let name = if raw.starts_with('#') {
                    "keyword"
                } else if raw.starts_with('[') {
                    "label"
                } else {
                    "string"
                };
                ranges.push((token.range(), color(name)));
            }
        }
        if ranges.is_empty() {
            return Self::default();
        }
        let starts = std::iter::once(0)
            .chain(body.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self { starts, ranges }
    }

    pub fn spans_in(&self, line: usize, text: &str, bytes: Range<usize>) -> Vec<Span<'static>> {
        let Some(&origin) = line.checked_sub(1).and_then(|i| self.starts.get(i)) else {
            return vec![Span::raw(terminal_text(&text[bytes]))];
        };
        let text = &text[bytes.clone()];
        let start = origin + bytes.start;
        let end = origin + bytes.end;
        let mut spans = Vec::new();
        let mut cursor = start;
        let first = self.ranges.partition_point(|(r, _)| r.end <= start);
        for (range, style) in &self.ranges[first..] {
            if range.start >= end {
                break;
            }
            let left = range.start.max(cursor);
            let right = range.end.min(end);
            if let (Some(gap), Some(token)) = (
                text.get(cursor - start..left - start),
                text.get(left - start..right - start),
            ) {
                if !gap.is_empty() {
                    spans.push(Span::raw(terminal_text(gap)));
                }
                spans.push(Span::styled(terminal_text(token), *style));
                cursor = right;
            }
        }
        if cursor < end || spans.is_empty() {
            spans.push(Span::raw(terminal_text(&text[cursor - start..])));
        }
        spans
    }
}

pub fn terminal_text(line: &str) -> String {
    line.replace('\t', "    ")
        .replace(|c: char| c.is_control(), "�")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requested_file_types_have_highlighted_tokens() {
        for (path, body, token, category) in [
            ("A.java", "public class A {}", "class", "keyword"),
            ("a.py", "def f(): return 4", "def", "keyword"),
            ("a.ts", "const value: number = 4;", "const", "keyword"),
            ("a.tsx", "const view = <div/>;", "const", "keyword"),
            ("a.js", "const label = 'text';", "const", "keyword"),
            ("a.yaml", "port: 8080", "port", "property"),
            ("a.yml", "enabled: false", "false", "boolean"),
            ("a.yaml", "port: 8080 # default", "# default", "comment"),
            ("a.md", "# 中文标题", "# 中文标题", "keyword"),
            ("a.xml", "<item name=\"中文\"/>", "name", "attribute"),
        ] {
            let rendered = Syntax::parse(path, Some(body)).spans_in(1, body, 0..body.len());
            assert!(
                rendered
                    .iter()
                    .any(|s| s.content == token && s.style == color(category)),
                "{path}: {rendered:?}"
            );
        }
    }
}
