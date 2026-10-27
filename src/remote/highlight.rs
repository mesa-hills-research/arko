//! The highlighting of the code blocks in the transcript's Markdown, for the
//! web page, which has no grammars of its own. A block is parsed with its
//! language's grammar (Mesquite's, as arko_index has them) and its spans come
//! from the highlight query GPUI Kit's code blocks use (in `highlight/`), so
//! code reads on the page as in the apps. The page paints the spans in the
//! theme's syntax colors.
//!
//! The blocks are found with the page's own Markdown parser, so both see the
//! same code, and the page finds a block's spans by its code's hash.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use tree_sitter::{Language, Query, QueryCursor};

use super::feed::{Item, Kind};
use super::protocol::{CodeSpans, HIGHLIGHT_NAMES, code_hash};

/// A language: the names a fence gives it, its grammar and its highlight
/// query.
struct Grammar {
    names: &'static [&'static str],
    language: fn() -> Language,
    query: &'static [&'static str],
}

/// The languages highlighted, with the names fences usually carry, as GPUI
/// Kit knows them.
const GRAMMARS: &[Grammar] = &[
    Grammar {
        names: &[
            "bash",
            "sh",
            "shell",
            "zsh",
            "ksh",
            "console",
            "shellsession",
            "shell-session",
        ],
        language: || tree_sitter_bash::LANGUAGE.into(),
        query: &[include_str!("highlight/bash.scm")],
    },
    Grammar {
        names: &["c", "h"],
        language: || tree_sitter_c::LANGUAGE.into(),
        query: &[include_str!("highlight/c.scm")],
    },
    Grammar {
        names: &["cpp", "c++", "cc", "cxx", "hpp", "hh", "hxx"],
        language: || tree_sitter_cpp::LANGUAGE.into(),
        query: &[include_str!("highlight/cpp.scm")],
    },
    Grammar {
        names: &["csharp", "cs", "c#"],
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        query: &[include_str!("highlight/csharp.scm")],
    },
    Grammar {
        names: &["css", "scss"],
        language: || tree_sitter_css::LANGUAGE.into(),
        query: &[include_str!("highlight/css.scm")],
    },
    Grammar {
        names: &["diff", "patch", "udiff"],
        language: || tree_sitter_diff::LANGUAGE.into(),
        query: &[include_str!("highlight/diff.scm")],
    },
    Grammar {
        names: &["go", "golang"],
        language: || tree_sitter_go::LANGUAGE.into(),
        query: &[include_str!("highlight/go.scm")],
    },
    Grammar {
        names: &["html", "htm", "xhtml"],
        language: || tree_sitter_html::LANGUAGE.into(),
        query: &[include_str!("highlight/html.scm")],
    },
    Grammar {
        names: &["java"],
        language: || tree_sitter_java::LANGUAGE.into(),
        query: &[include_str!("highlight/java.scm")],
    },
    Grammar {
        names: &["javascript", "js", "jsx", "mjs", "cjs"],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        query: &[include_str!("highlight/javascript.scm")],
    },
    Grammar {
        names: &["json", "jsonc", "json5", "jsonl"],
        language: || tree_sitter_json::LANGUAGE.into(),
        query: &[include_str!("highlight/json.scm")],
    },
    Grammar {
        names: &["kotlin", "kt", "kts", "ktm"],
        language: || tree_sitter_kotlin_ng::LANGUAGE.into(),
        query: &[include_str!("highlight/kotlin.scm")],
    },
    Grammar {
        names: &["lua"],
        language: || tree_sitter_lua::LANGUAGE.into(),
        query: &[include_str!("highlight/lua.scm")],
    },
    Grammar {
        names: &["make", "makefile", "mk"],
        language: || tree_sitter_make::LANGUAGE.into(),
        query: &[include_str!("highlight/make.scm")],
    },
    Grammar {
        names: &["php", "php3", "php4", "php5", "phtml"],
        language: || tree_sitter_php::LANGUAGE_PHP.into(),
        query: &[include_str!("highlight/php.scm")],
    },
    Grammar {
        names: &["python", "py", "python3", "py3"],
        language: || tree_sitter_python::LANGUAGE.into(),
        query: &[include_str!("highlight/python.scm")],
    },
    Grammar {
        names: &["ruby", "rb"],
        language: || tree_sitter_ruby::LANGUAGE.into(),
        query: &[include_str!("highlight/ruby.scm")],
    },
    Grammar {
        names: &["rust", "rs"],
        language: || tree_sitter_rust::LANGUAGE.into(),
        query: &[include_str!("highlight/rust.scm")],
    },
    Grammar {
        names: &["sql", "postgresql", "postgres", "pgsql", "mysql", "sqlite"],
        language: || tree_sitter_sequel::LANGUAGE.into(),
        query: &[include_str!("highlight/sql.scm")],
    },
    Grammar {
        names: &["swift"],
        language: || tree_sitter_swift::LANGUAGE.into(),
        query: &[include_str!("highlight/swift.scm")],
    },
    Grammar {
        names: &["toml"],
        language: || tree_sitter_toml_ng::LANGUAGE.into(),
        query: &[include_str!("highlight/toml.scm")],
    },
    Grammar {
        names: &["typescript", "ts", "mts", "cts"],
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        query: &[include_str!("highlight/typescript.scm")],
    },
    // TSX is TypeScript with markup, so its query follows TypeScript's.
    Grammar {
        names: &["tsx"],
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        query: &[
            include_str!("highlight/typescript.scm"),
            include_str!("highlight/tsx.scm"),
        ],
    },
    Grammar {
        names: &["yaml", "yml"],
        language: || tree_sitter_yaml::LANGUAGE.into(),
        query: &[include_str!("highlight/yaml.scm")],
    },
];

/// A code block larger than this shows plain, to keep a stream's work small.
const LARGEST: usize = 64 * 1024;

/// The spans worked out, by the code's hash, kept until there are this many.
const KEPT: usize = 4096;

/// A language's grammar with its query compiled, and the highlight name of
/// each of the query's captures.
struct Compiled {
    language: Language,
    query: Query,
    names: Vec<Option<u32>>,
}

/// Each language's [`Compiled`], once a block of it came (`None` if its
/// query didn't compile).
static COMPILED: LazyLock<Mutex<HashMap<usize, Option<Arc<Compiled>>>>> =
    LazyLock::new(Mutex::default);
/// The spans of code blocks, by their code's hash.
type Kept = HashMap<String, Arc<Vec<[u32; 3]>>>;

static SPANS: LazyLock<Mutex<Kept>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The language a fence's first word names.
fn grammar(fence: &str) -> Option<usize> {
    let name = fence.split_whitespace().next()?.to_lowercase();
    GRAMMARS
        .iter()
        .position(|grammar| grammar.names.contains(&name.as_str()))
}

/// The highlight name of the capture `name`: its own, else its first
/// word's, else none (captures that only feed a predicate, or name no
/// style).
fn highlight_name(name: &str) -> Option<u32> {
    if name.starts_with('_') {
        return None;
    }
    let find = |name: &str| HIGHLIGHT_NAMES.iter().position(|known| *known == name);
    find(name)
        .or_else(|| find(name.split('.').next().unwrap_or(name)))
        .map(|at| at as u32)
}

fn compiled(at: usize) -> Option<Arc<Compiled>> {
    if let Some(done) = lock(&COMPILED).get(&at) {
        return done.clone();
    }
    // (Compiled outside the lock: a grammar's tables decode the first time,
    // which takes tens of milliseconds.)
    let grammar = &GRAMMARS[at];
    let language = (grammar.language)();
    let compiled = match Query::new(&language, &grammar.query.concat()) {
        Ok(query) => {
            let names = query
                .capture_names()
                .iter()
                .map(|name| highlight_name(name))
                .collect();
            Some(Arc::new(Compiled {
                language,
                query,
                names,
            }))
        }
        Err(error) => {
            log::warn!(
                "the highlight query of {} didn't compile: {error}",
                grammar.names[0]
            );
            None
        }
    };
    lock(&COMPILED).insert(at, compiled.clone());
    compiled
}

/// The spans of `code` in the language `at`: `[start, end, name]` in bytes.
/// Of the captures of one node the first pattern's wins, as in
/// tree-sitter's highlighter, and a node's inside takes its own.
fn spans(at: usize, code: &str) -> Vec<[u32; 3]> {
    let Some(compiled) = compiled(at) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&compiled.language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(code, None) else {
        return Vec::new();
    };
    let mut captures = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&compiled.query, tree.root_node(), code.as_bytes());
    while let Some(found) = matches.next() {
        for capture in &found.captures {
            if let Some(name) = compiled
                .names
                .get(capture.index as usize)
                .copied()
                .flatten()
            {
                captures.push((capture.node.byte_range(), found.pattern_index, name));
            }
        }
    }
    // Painted from the outside in, and for one node the first pattern last.
    captures.sort_by_key(|(range, pattern, _)| (Reverse(range.len()), Reverse(*pattern)));
    let mut painted = vec![u32::MAX; code.len()];
    for (range, _, name) in captures {
        if range.end <= painted.len() {
            painted[range].fill(name);
        }
    }
    let mut spans: Vec<[u32; 3]> = Vec::new();
    for (at, name) in painted.into_iter().enumerate() {
        if name == u32::MAX {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last[1] == at as u32 && last[2] == name => last[1] += 1,
            _ => spans.push([at as u32, at as u32 + 1, name]),
        }
    }
    spans
}

/// The spans of a block of `code` in the language its fence names, worked
/// out once, or none for a language without a grammar here.
fn block(fence: &str, code: &str) -> Option<CodeSpans> {
    let at = grammar(fence)?;
    if code.len() > LARGEST {
        return None;
    }
    let hash = code_hash(code);
    let known = lock(&SPANS).get(&hash).cloned();
    let spans = match known {
        Some(spans) => spans,
        None => {
            let spans = Arc::new(spans(at, code));
            let mut kept = lock(&SPANS);
            if kept.len() >= KEPT {
                kept.clear();
            }
            kept.insert(hash.clone(), spans.clone());
            spans
        }
    };
    Some(CodeSpans {
        hash,
        spans: spans.to_vec(),
    })
}

/// The code blocks of `markdown` highlighted. With `finished`, those whose
/// fence has closed only, as a text that streams has them.
pub fn code_blocks(markdown: &str, finished: bool) -> Vec<CodeSpans> {
    let mut found = Vec::new();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut open: Option<(String, String, usize)> = None;
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(fence))) => {
                open = Some((fence.to_string(), String::new(), range.start));
            }
            Event::Text(text) => {
                if let Some((_, code, _)) = &mut open {
                    code.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                let Some((fence, code, start)) = open.take() else {
                    continue;
                };
                if finished && !closes(&markdown[start..range.end]) {
                    continue;
                }
                found.extend(block(&fence, &code));
            }
            _ => {}
        }
    }
    found
}

/// Whether a fenced block's source ends with its closing fence (one that a
/// text still streaming may not have yet).
fn closes(block: &str) -> bool {
    let mut lines = block.trim_end().lines();
    let opening = lines.next().map(str::trim_start).unwrap_or("");
    let Some(mark) = opening.chars().next().filter(|c| *c == '`' || *c == '~') else {
        return false;
    };
    let length = opening.chars().take_while(|c| *c == mark).count();
    lines.next_back().is_some_and(|last| {
        let last = last.trim();
        last.chars().all(|c| c == mark) && last.chars().count() >= length
    })
}

/// The code blocks of an item's Markdown highlighted, for the page. A text
/// or thinking may still stream, so only the blocks it finished count.
pub fn item_code(item: &Item) -> Vec<CodeSpans> {
    match item.kind {
        Kind::Text | Kind::Thinking => code_blocks(&item.text, true),
        Kind::Reply | Kind::Message | Kind::Question => code_blocks(&item.text, false),
        _ => Vec::new(),
    }
}

/// Whether one of the whole lines that the last `added` bytes of `text`
/// ended may close a code fence, after which a text that streams may have a
/// block more to highlight.
pub fn fence_line_ended(text: &str, added: usize) -> bool {
    let start = text.len().saturating_sub(added);
    let from = text[..start].rfind('\n').map_or(0, |at| at + 1);
    let Some(end) = text.rfind('\n').filter(|end| *end >= start) else {
        return false;
    };
    text[from..end].lines().any(|line| {
        let line = line.trim();
        line.len() >= 3 && (line.chars().all(|c| c == '`') || line.chars().all(|c| c == '~'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every language's query compiles with its grammar.
    #[test]
    fn every_languages_query_compiles() {
        for (at, grammar) in GRAMMARS.iter().enumerate() {
            assert!(compiled(at).is_some(), "{}", grammar.names[0]);
        }
    }

    fn named(code: &str, spans: &[[u32; 3]]) -> Vec<(String, &'static str)> {
        spans
            .iter()
            .map(|[start, end, name]| {
                (
                    code[*start as usize..*end as usize].to_string(),
                    HIGHLIGHT_NAMES[*name as usize],
                )
            })
            .collect()
    }

    #[test]
    fn a_rust_blocks_keywords_strings_and_functions_are_named() {
        let markdown = "Here:\n\n```rust\nfn main() {\n    let name = \"Arko\"; // hi\n}\n```\n";
        let blocks = code_blocks(markdown, true);
        assert_eq!(blocks.len(), 1);
        let code = "fn main() {\n    let name = \"Arko\"; // hi\n}\n";
        assert_eq!(blocks[0].hash, code_hash(code));
        let named = named(code, &blocks[0].spans);
        for expected in [
            ("fn", "keyword"),
            ("main", "function"),
            ("let", "keyword"),
            ("\"Arko\"", "string"),
            ("// hi", "comment"),
        ] {
            assert!(
                named.contains(&(expected.0.to_string(), expected.1)),
                "{expected:?} in {named:?}"
            );
        }
    }

    #[test]
    fn a_block_still_open_waits_and_an_unknown_language_shows_plain() {
        assert!(code_blocks("```python\nprint(1)\n", true).is_empty());
        assert_eq!(code_blocks("```python\nprint(1)\n", false).len(), 1);
        assert_eq!(code_blocks("```python\nprint(1)\n```\n", true).len(), 1);
        assert_eq!(
            code_blocks("  - a list\n\n    ```py\n    x = 1\n    ```\n", true).len(),
            1
        );
        assert!(code_blocks("```brainfuck\n+++\n```\n", false).is_empty());
        assert!(code_blocks("```\nplain\n```\n", false).is_empty());
    }

    #[test]
    fn a_capture_takes_its_own_highlight_name_else_its_first_words() {
        let name = |capture| highlight_name(capture).map(|at| HIGHLIGHT_NAMES[at as usize]);
        assert_eq!(name("string.escape"), Some("string.escape"));
        assert_eq!(name("function.method.call"), Some("function"));
        assert_eq!(name("_lhs"), None);
        assert_eq!(name("spell"), None);
    }

    #[test]
    fn a_line_of_backticks_or_tildes_that_ends_may_close_a_fence() {
        assert!(fence_line_ended("x\n```\n", 4));
        assert!(fence_line_ended("x\n```\nnext", 9));
        assert!(!fence_line_ended("x\n``", 2));
        assert!(fence_line_ended("x\n```\n~~~~\n", 1));
        assert!(!fence_line_ended("x\n```\nmore\n", 5));
        assert!(!fence_line_ended("```rust\n", 8));
    }
}
