//! Heuristic project-wide symbol index for the "Go to symbol" popup.
//!
//! This crate has no LSP client — Fresh owns the language servers — so there is
//! no `workspace/symbol` to ask. What ships instead is a NAME index: a
//! per-language table of definition regexes applied to the files the SAME
//! `ignore` walk Quick Open uses. Results are "lines that look like a
//! definition", not a semantic symbol graph: a macro-generated symbol, a
//! re-export, or the real owner of an `impl Trait for X` method can be wrong or
//! missing. See `docs/herdr-ide.md` for the user-facing caveat.
//!
//! Everything here is pure (no TUI, no IPC) so the popup can run the index on a
//! worker thread and the ranking on every keystroke.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::file_search::{QUICK_OPEN_FILE_LIMIT, collect_quick_files, fuzzy_score_lowercased};

/// Symbols kept in the in-memory index. 5000 is far beyond what anyone scrolls
/// and keeps the per-keystroke ranking trivial even on a monorepo.
pub const SYMBOL_LIMIT: usize = 5_000;
/// A source file bigger than this is skipped: generated bundles and data dumps
/// are never what a symbol jump is looking for.
pub const SYMBOL_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Class,
    Interface,
    Trait,
    Enum,
    Module,
    Type,
    Constant,
    Variable,
    Macro,
    Heading,
}

impl SymbolKind {
    /// Short label drawn next to the name. Deliberately three cells max so the
    /// name never gets pushed off a narrow popup.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Struct => "str",
            SymbolKind::Class => "cls",
            SymbolKind::Interface => "ifc",
            SymbolKind::Trait => "trt",
            SymbolKind::Enum => "enm",
            SymbolKind::Module => "mod",
            SymbolKind::Type => "typ",
            SymbolKind::Constant => "cst",
            SymbolKind::Variable => "var",
            SymbolKind::Macro => "mac",
            SymbolKind::Heading => "h",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub name_lower: String,
    /// Root-relative, `/`-separated, for display and path matching.
    pub label: String,
    pub label_lower: String,
    pub path: PathBuf,
    /// 1-based line of the definition.
    pub line: usize,
    pub kind: SymbolKind,
}

struct LangRules {
    extensions: &'static [&'static str],
    rules: &'static [(SymbolKind, &'static str)],
}

/// Definition patterns per language family. First matching rule on a line wins,
/// so a line reports one symbol. Every pattern MUST capture the name in group 1.
const LANGUAGES: &[LangRules] = &[
    LangRules {
        extensions: &["rs"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\x22[^\x22]*\x22\s+)?fn\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Struct,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Enum,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Trait,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?trait\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Module,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Type,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Constant,
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+(?:mut\s+)?([A-Za-z_]\w*)",
            ),
            (SymbolKind::Macro, r"^\s*macro_rules!\s*([A-Za-z_]\w*)"),
        ],
    },
    LangRules {
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
        rules: &[
            (
                SymbolKind::Class,
                r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)",
            ),
            (
                SymbolKind::Interface,
                r"^\s*(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)",
            ),
            (
                SymbolKind::Enum,
                r"^\s*(?:export\s+)?(?:declare\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)",
            ),
            (
                SymbolKind::Type,
                r"^\s*(?:export\s+)?(?:declare\s+)?type\s+([A-Za-z_$][\w$]*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:function\b|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)",
            ),
            (
                SymbolKind::Variable,
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)",
            ),
        ],
    },
    LangRules {
        extensions: &["py", "pyi"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)",
            ),
            (SymbolKind::Class, r"^\s*class\s+([A-Za-z_]\w*)"),
        ],
    },
    LangRules {
        extensions: &["go"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)",
            ),
            (SymbolKind::Type, r"^\s*type\s+([A-Za-z_]\w*)"),
            (SymbolKind::Variable, r"^\s*(?:var|const)\s+([A-Za-z_]\w*)"),
        ],
    },
    LangRules {
        extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm"],
        rules: &[
            (
                SymbolKind::Struct,
                r"^\s*(?:typedef\s+)?(?:struct|class|union|namespace|enum)\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Function,
                // A definition ends the signature with `{`; a call ends with `;`,
                // which is what keeps `return foo(x);` out of the index.
                r"^\s*(?:template\s*<[^>]*>\s*)?(?:[\w:]+[\s\*&]+)+([A-Za-z_]\w*)\s*\([^;{}]*\)\s*(?:const\s*)?(?:noexcept\s*)?\{",
            ),
        ],
    },
    LangRules {
        extensions: &["java", "cs", "scala", "kt", "kts", "groovy"],
        rules: &[
            (
                SymbolKind::Class,
                r"^\s*(?:@\w+\s*)*(?:public|private|protected|internal|static|final|abstract|open|sealed|data|partial|async|\s)*(?:class|interface|enum|record|struct|object|trait)\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:@\w+\s*)*(?:public|private|protected|internal|override|suspend|inline|open|operator|static|final|abstract|virtual|async|synchronized|native|\s)*fun\s+(?:<[^>]*>\s*)?([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:@\w+\s*)*(?:public|private|protected|internal|static|final|abstract|override|virtual|async|synchronized|\s)+[\w<>\[\],\.\?]+\s+([A-Za-z_]\w*)\s*\([^;]*\)\s*(?:throws[^{]*)?\{",
            ),
        ],
    },
    LangRules {
        extensions: &["rb"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*def\s+(?:self\.)?([A-Za-z_]\w*[?!=]?)",
            ),
            (SymbolKind::Class, r"^\s*(?:class|module)\s+([A-Za-z_]\w*)"),
        ],
    },
    LangRules {
        extensions: &["php"],
        rules: &[
            (
                SymbolKind::Class,
                r"^\s*(?:abstract\s+|final\s+|readonly\s+)*(?:class|interface|trait|enum)\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:public|private|protected|static|abstract|final|\s)*function\s+&?([A-Za-z_]\w*)",
            ),
        ],
    },
    LangRules {
        extensions: &["sh", "bash", "zsh"],
        rules: &[(
            SymbolKind::Function,
            r"^\s*(?:function\s+)?([A-Za-z_][\w-]*)\s*\(\)\s*\{",
        )],
    },
    LangRules {
        extensions: &["lua"],
        rules: &[(
            SymbolKind::Function,
            r"^\s*(?:local\s+)?function\s+([\w.:]+)",
        )],
    },
    LangRules {
        extensions: &["zig"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*(?:pub\s+)?(?:export\s+)?(?:inline\s+)?fn\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Type,
                r"^\s*(?:pub\s+)?const\s+([A-Za-z_]\w*)\s*=\s*(?:packed\s+)?(?:struct|union|enum|opaque)\b",
            ),
        ],
    },
    LangRules {
        extensions: &["swift"],
        rules: &[
            (
                SymbolKind::Function,
                r"^\s*(?:@\w+\s*)*(?:public|private|internal|fileprivate|open|static|class|final|override|mutating|\s)*func\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Class,
                r"^\s*(?:public|private|internal|open|final|\s)*(?:class|struct|enum|protocol|extension|actor)\s+([A-Za-z_]\w*)",
            ),
        ],
    },
    LangRules {
        extensions: &["dart"],
        rules: &[
            (
                SymbolKind::Class,
                r"^\s*(?:abstract\s+|sealed\s+|base\s+|final\s+)*(?:class|mixin|enum|extension|typedef)\s+([A-Za-z_]\w*)",
            ),
            (
                SymbolKind::Function,
                r"^\s*(?:static\s+)?[\w<>\[\],\?]+\s+([A-Za-z_]\w*)\s*\([^;]*\)\s*(?:async\s*)?\{",
            ),
        ],
    },
    LangRules {
        extensions: &["ex", "exs"],
        rules: &[(
            SymbolKind::Function,
            r"^\s*def(?:p|module|macro|struct|impl|delegate|exception)?\s+([A-Za-z_]\w*[?!]?)",
        )],
    },
    LangRules {
        extensions: &["hs", "lhs"],
        rules: &[
            (SymbolKind::Function, r"^([a-z_]\w*)\s*::"),
            (
                SymbolKind::Type,
                r"^\s*(?:data|newtype|type|class)\s+(?:[A-Za-z_]+\s+)*([A-Z]\w*)",
            ),
        ],
    },
    LangRules {
        extensions: &["md", "markdown"],
        rules: &[(SymbolKind::Heading, r"^#{1,6}\s+([^#\s].*?)\s*#*\s*$")],
    },
];

/// Compiled patterns, index-aligned with [`LANGUAGES`]. Compiling once instead
/// of per file keeps a 20k-file walk cheap.
fn compiled() -> &'static [Vec<(SymbolKind, Regex)>] {
    static COMPILED: OnceLock<Vec<Vec<(SymbolKind, Regex)>>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        LANGUAGES
            .iter()
            .map(|language| {
                language
                    .rules
                    .iter()
                    .map(|(kind, pattern)| {
                        let regex = Regex::new(pattern)
                            .unwrap_or_else(|error| panic!("symbol pattern {pattern}: {error}"));
                        (*kind, regex)
                    })
                    .collect()
            })
            .collect()
    })
}

fn rules_for(path: &Path) -> Option<&'static [(SymbolKind, Regex)]> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let index = LANGUAGES
        .iter()
        .position(|language| language.extensions.contains(&extension.as_str()))?;
    Some(&compiled()[index])
}

/// Index `root` on the same walk Quick Open uses (gitignore-aware, `.git`
/// skipped, no directory symlinks). Returns the symbols plus whether a cap cut
/// the walk short.
pub fn collect_symbols(root: &Path, show_hidden: bool, limit: usize) -> (Vec<Symbol>, bool) {
    let (files, files_truncated) = collect_quick_files(root, show_hidden, QUICK_OPEN_FILE_LIMIT);
    let mut symbols = Vec::new();
    let mut truncated = files_truncated;
    for file in files {
        if rules_for(&file.path).is_none() {
            continue;
        }
        if std::fs::metadata(&file.path)
            .map(|metadata| metadata.len() > SYMBOL_MAX_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file.path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        truncated |= collect_from_text(&file.path, &file.label, text, limit, &mut symbols);
        if symbols.len() >= limit {
            truncated = true;
            break;
        }
    }
    (symbols, truncated)
}
/// Loose C-family patterns can otherwise report a control-flow keyword as the
/// definition name (`else if (x) {` is not a function).
const NOT_NAMES: &[&str] = &[
    "if", "else", "for", "foreach", "while", "switch", "catch", "return", "do", "try", "new",
    "await", "sizeof", "typeof", "function", "case", "match",
];

/// Scan one file's text, appending to `symbols`. Returns true when `limit` was
/// hit. Split out so the table is testable without touching the filesystem.
fn collect_from_text(
    path: &Path,
    label: &str,
    text: &str,
    limit: usize,
    symbols: &mut Vec<Symbol>,
) -> bool {
    let Some(rules) = rules_for(path) else {
        return false;
    };
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        for (kind, regex) in rules {
            let Some(captures) = regex.captures(line) else {
                continue;
            };
            let Some(name) = captures.get(1).map(|m| m.as_str().trim()) else {
                continue;
            };
            if name.is_empty() || NOT_NAMES.contains(&name) {
                continue;
            }
            symbols.push(Symbol {
                name: name.to_string(),
                name_lower: name.to_lowercase(),
                label: label.to_string(),
                label_lower: label.to_lowercase(),
                path: path.to_path_buf(),
                line: index + 1,
                kind: *kind,
            });
            if symbols.len() >= limit {
                truncated = true;
            }
            // One symbol per line: the next rule would only re-report it.
            break;
        }
        if truncated {
            break;
        }
    }
    truncated
}

/// Rank the index against `query`. An empty query keeps the walk's own order
/// (path, then line), which is what a first look at the popup should show.
/// A name match outweighs a path match, so typing `parse` finds the function
/// before every file under `parser/`.
pub fn match_symbols(symbols: &[Symbol], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..symbols.len()).collect();
    }
    let query_lower = query.to_lowercase();
    let mut ranked = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            let name = fuzzy_score_lowercased(&query_lower, &symbol.name_lower).map(|s| s * 3);
            let path = fuzzy_score_lowercased(&query_lower, &symbol.label_lower);
            let score = match (name, path) {
                (Some(name), Some(path)) => name.max(path),
                (Some(name), None) => name,
                (None, Some(path)) => path,
                (None, None) => return None,
            };
            Some((index, score))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| {
                symbols[*left_index]
                    .name
                    .len()
                    .cmp(&symbols[*right_index].name.len())
            })
            .then_with(|| symbols[*left_index].label.cmp(&symbols[*right_index].label))
            .then_with(|| symbols[*left_index].line.cmp(&symbols[*right_index].line))
    });
    ranked.into_iter().map(|(index, _)| index).collect()
}

/// Char indices of `name` matched by the greedy subsequence `query_lower`, for
/// highlighting the hit. Empty when it does not match.
pub fn match_positions(query_lower: &str, name: &str) -> Vec<usize> {
    let mut wanted = query_lower.chars();
    let Some(mut current) = wanted.next() else {
        return Vec::new();
    };
    let mut positions = Vec::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.to_lowercase().eq(std::iter::once(current)) {
            positions.push(index);
            match wanted.next() {
                Some(next) => current = next,
                None => return positions,
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(label: &str, text: &str) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        collect_from_text(Path::new(label), label, text, SYMBOL_LIMIT, &mut symbols);
        symbols
    }

    fn names(symbols: &[Symbol]) -> Vec<(&str, usize, SymbolKind)> {
        symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.line, symbol.kind))
            .collect()
    }

    #[test]
    fn every_language_pattern_compiles() {
        compiled();
    }

    #[test]
    fn rust_definitions_are_indexed_with_their_lines() {
        let text = "\
use std::io;

pub struct Sidebar {
    root: PathBuf,
}

enum View { Explorer }

trait Repo {}

const LIMIT: usize = 3;

macro_rules! bail {
}

impl Sidebar {
    pub(crate) async fn refresh(&mut self) {}
    unsafe fn raw() {}
}
";
        assert_eq!(
            names(&index("src/app.rs", text)),
            vec![
                ("Sidebar", 3, SymbolKind::Struct),
                ("View", 7, SymbolKind::Enum),
                ("Repo", 9, SymbolKind::Trait),
                ("LIMIT", 11, SymbolKind::Constant),
                ("bail", 13, SymbolKind::Macro),
                ("refresh", 17, SymbolKind::Function),
                ("raw", 18, SymbolKind::Function),
            ]
        );
    }

    #[test]
    fn typescript_functions_classes_and_arrow_consts_are_indexed() {
        let text = "\
export interface Options { a: number }
export type Key = string;
export const enum Mode { A }
export class App {
  async render() {}
}
export async function load() {}
const handler = async (event: Event) => { event };
const plain = 4;
";
        assert_eq!(
            names(&index("src/app.ts", text)),
            vec![
                ("Options", 1, SymbolKind::Interface),
                ("Key", 2, SymbolKind::Type),
                ("Mode", 3, SymbolKind::Enum),
                ("App", 4, SymbolKind::Class),
                ("load", 7, SymbolKind::Function),
                ("handler", 8, SymbolKind::Function),
                ("plain", 9, SymbolKind::Variable),
            ]
        );
    }

    #[test]
    fn python_go_ruby_shell_and_markdown_are_indexed() {
        assert_eq!(
            names(&index("a.py", "class Box:\n    async def run(self):\n")),
            vec![
                ("Box", 1, SymbolKind::Class),
                ("run", 2, SymbolKind::Function)
            ]
        );
        assert_eq!(
            names(&index(
                "a.go",
                "type Server struct{}\nfunc (s *Server) Start() {}\n"
            )),
            vec![
                ("Server", 1, SymbolKind::Type),
                ("Start", 2, SymbolKind::Function)
            ]
        );
        assert_eq!(
            names(&index("a.rb", "module Api\n  def call!\n")),
            vec![
                ("Api", 1, SymbolKind::Class),
                ("call!", 2, SymbolKind::Function)
            ]
        );
        assert_eq!(
            names(&index("a.sh", "build_matrix() {\n  :\n}\n")),
            vec![("build_matrix", 1, SymbolKind::Function)]
        );
        assert_eq!(
            names(&index("docs/a.md", "# Title\n\n## Sub section ##\n")),
            vec![
                ("Title", 1, SymbolKind::Heading),
                ("Sub section", 3, SymbolKind::Heading)
            ]
        );
    }

    #[test]
    fn c_calls_are_not_mistaken_for_definitions() {
        let text = "\
int main(int argc, char **argv) { return 0; }
void other(void) {
    return do_work(argc);
}
struct point;
";
        assert_eq!(
            names(&index("src/main.c", text)),
            vec![
                ("main", 1, SymbolKind::Function),
                ("other", 2, SymbolKind::Function),
                ("point", 5, SymbolKind::Struct),
            ]
        );
    }

    #[test]
    fn unknown_extensions_and_binaries_index_nothing() {
        assert!(index("data.json", "{\"name\": \"x\"}\n").is_empty());
        assert!(index("blob.png", "\u{0}fn nope() {}\n").is_empty());
    }

    #[test]
    fn indexing_stops_at_the_symbol_limit() {
        let mut symbols = Vec::new();
        let truncated = collect_from_text(
            Path::new("a.rs"),
            "a.rs",
            "fn one() {}\nfn two() {}\nfn three() {}\n",
            2,
            &mut symbols,
        );
        assert!(truncated);
        assert_eq!(symbols.len(), 2);
    }

    #[test]
    fn walk_indexes_a_repository_and_skips_gitignored_files() {
        let root =
            std::env::temp_dir().join(format!("herdr-sidebar-symbols-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn real() {}\n").unwrap();
        std::fs::write(root.join("target/gen.rs"), "pub fn generated() {}\n").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let (symbols, truncated) = collect_symbols(&root, false, SYMBOL_LIMIT);
        let _ = std::fs::remove_dir_all(&root);
        assert!(!truncated);
        assert_eq!(
            names(&symbols),
            vec![("real", 1, SymbolKind::Function)],
            "gitignored paths must not be indexed"
        );
    }

    fn symbol(name: &str, label: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            name_lower: name.to_lowercase(),
            label: label.to_string(),
            label_lower: label.to_lowercase(),
            path: PathBuf::from(label),
            line,
            kind: SymbolKind::Function,
        }
    }

    #[test]
    fn ranking_prefers_the_name_over_the_path() {
        let symbols = vec![
            // Matches only through `src/parser/`.
            symbol("handle", "src/parser/mod.rs", 1),
            // Matches through its own name.
            symbol("parse_expression", "src/editor.rs", 2),
        ];
        // An empty query keeps walk order.
        assert_eq!(match_symbols(&symbols, ""), vec![0, 1]);
        assert_eq!(match_symbols(&symbols, "parse"), vec![1, 0]);
        assert_eq!(match_symbols(&symbols, "handle"), vec![0]);
        assert!(match_symbols(&symbols, "zzz").is_empty());
    }

    #[test]
    fn ranking_prefers_the_shorter_name_when_both_match_equally() {
        let symbols = vec![
            symbol("parse_expression", "src/editor.rs", 2),
            symbol("Parser", "src/parser/mod.rs", 9),
        ];
        // `Parser` is a five-char prefix match; the longer name loses the
        // fuzzy scorer's length tiebreak and comes second.
        assert_eq!(match_symbols(&symbols, "parse"), vec![1, 0]);
    }

    #[test]
    fn match_positions_track_the_greedy_subsequence() {
        assert_eq!(match_positions("pt", "parse_token"), vec![0, 6]);
        assert_eq!(match_positions("abc", "abc"), vec![0, 1, 2]);
        assert_eq!(match_positions("", "anything"), Vec::<usize>::new());
        assert!(match_positions("zz", "parse").is_empty());
        // Case-insensitive on the candidate side.
        assert_eq!(match_positions("st", "StartUp"), vec![0, 1]);
    }
}
