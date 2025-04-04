use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::iter;
use std::path::{Path, PathBuf};

pub use config::Config;
use config::NodeType;
use languagetool_rust::check::DataAnnotation;
use lsp_types::Position;
use tree_sitter::{Language as Grammar, Query, QueryCursor, Tree};

use super::Result;
use crate::lsp::Context as _;

mod config;

impl Config {
    pub fn find_languages(&self) -> impl Iterator<Item = Result<Language>> + '_ {
        self.grammars.iter().flat_map(|g| {
            if g.is_file() {
                Box::new(iter::once(Language::load(g)))
                    as Box<dyn Iterator<Item = Result<Language>>>
            } else if g.is_dir() {
                match g
                    .read_dir()
                    .invalid_request(format_args!("unable to read dir `{}`", g.display()))
                {
                    Ok(d) => Box::new(d.map(move |f| {
                        match f.internal_error(format_args!("reading dir: {g:?}")) {
                            Ok(f) => Language::load(&f.path()),
                            Err(e) => Err(e),
                        }
                    })) as Box<dyn Iterator<Item = Result<Language>>>,
                    Err(e) => Box::new(iter::once(Err(e))),
                }
            } else {
                Box::new(iter::once(Err(invalid_params!(
                    "unable to read libraries at `{g:?}`"
                ))))
            }
        })
    }
}

#[derive(Clone)]
pub struct Language {
    pub name: String,
    grammar: Grammar,
}

impl Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Language").field(&self.name).finish()
    }
}

impl Language {
    pub fn load(p: &Path) -> Result<Self> {
        use libloading::{Library, Symbol};

        let file_name = p
            .file_name()
            .internal_error(format_args!(
                "loading grammar, {p:?} didn't have a file_name"
            ))?
            .to_string_lossy();
        let name = file_name
            .rsplit_once('.')
            .internal_error(format_args!(
                "loading grammar, {p:?}'s file name doesn't contain a `.`"
            ))?
            .0;

        let library = unsafe { Library::new(p) }
            .internal_error(format_args!("Error opening dynamic library {p:?}"))?;
        let language_fn_name = format!("tree_sitter_{}", name.replace('-', "_"));
        let grammar = unsafe {
            let language_fn: Symbol<unsafe extern "C" fn() -> Grammar> = library
                .get(language_fn_name.as_bytes())
                .internal_error(format_args!("Failed to load symbol {language_fn_name}"))?;
            language_fn()
        };
        std::mem::forget(library);
        Ok(Language {
            name: name.to_owned(),
            grammar,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // TODO plattform specifics
            grammars: ["/usr/lib/helix/runtime/grammars", "/usr/lib/tree_sitter"]
                .map(PathBuf::from)
                .to_vec(),
            languages: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Document {
    text: String,
    tree: Tree,
    pub language: Language,
}

pub struct Segment {
    pub text: String,
    r#type: NodeType,
    /// Maps the start of ranges.
    ranges: BTreeMap<usize, usize>,
}

impl Document {
    pub fn spell_checkable(&self, config: &Config) -> Vec<Segment> {
        if let Some(config::Language { nodes }) = config.languages.get(&self.language.name) {
            nodes
                .iter()
                .flat_map(|n| {
                    // TODO memoize
                    eprintln!("{n:?}");
                    let query = Query::new(&self.tree.language(), &n.query).unwrap();
                    let names = query.capture_names();
                    let ignored: BTreeSet<_> = names
                        .iter()
                        .enumerate()
                        .filter_map(|(i, n)| n.starts_with('_').then_some(i))
                        .collect();
                    let ignored = &ignored;
                    let mut cursor = QueryCursor::new();
                    cursor
                        .matches(&query, self.tree.root_node(), self.text.as_bytes())
                        .map(|c| {
                            let mut text = String::new();
                            let mut ranges = BTreeMap::new();
                            for capture in c
                                .captures
                                .iter()
                                .filter(|c| !ignored.contains(&(c.index as usize)))
                            {
                                let start_document = capture.node.start_byte();
                                // TODO apply regex
                                ranges.insert(text.len(), start_document);
                                text.push_str(&self.text[start_document..capture.node.end_byte()]);
                                text.push('\n');
                            }
                            Segment {
                                text,
                                r#type: n.r#type,
                                ranges,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        } else {
            vec![Segment {
                text: self.text.clone(),
                r#type: NodeType::Text,
                ranges: iter::once((0, 0)).collect(),
            }]
        }
    }

    pub fn new(text: String, language: Language) -> Self {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language.grammar).unwrap();

        Self {
            tree: parser.parse(&text, None).expect("WTWTWTWTF"),
            text,
            language,
        }
    }
}

impl Segment {
    pub fn tag_markup(&self) -> Vec<DataAnnotation> {
        if self.r#type == NodeType::Text {
            return vec![DataAnnotation::new_text(self.text.clone())];
        }
        let mut parser = pulldown_cmark::Parser::new(&self.text)
            .into_offset_iter()
            .peekable();
        let mut in_code_block = 0usize;
        let mut last = 0;
        let mut tokens = Vec::new();
        while let Some((event, mut range)) = parser.next() {
            if range.start > last {
                tokens.push(DataAnnotation::new_markup(
                    self.text[last..range.start].to_owned(),
                ));
            } else {
                range.start = range.start.max(last);
            }
            if matches!(event, pulldown_cmark::Event::Start(_)) {
                range.end = parser.peek().map_or(range.end, |e| e.1.start);
            }
            last = range.end;
            let content = self.text[range].to_owned();
            tokens.push(match event {
                pulldown_cmark::Event::Text(_) if in_code_block == 0 => {
                    DataAnnotation::new_text(content)
                }
                pulldown_cmark::Event::SoftBreak => {
                    DataAnnotation::new_interpreted_markup(content, " ".to_owned())
                }
                pulldown_cmark::Event::HardBreak => {
                    DataAnnotation::new_interpreted_markup(content, "\n\n".to_owned())
                }
                pulldown_cmark::Event::Code(_) => {
                    DataAnnotation::new_interpreted_markup(content, "0".into())
                }
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading { .. }) => {
                    DataAnnotation::new_interpreted_markup(content, "Heading: ".into())
                }
                pulldown_cmark::Event::End(
                    pulldown_cmark::TagEnd::Paragraph
                    | pulldown_cmark::TagEnd::Heading(_)
                    | pulldown_cmark::TagEnd::List(_)
                    | pulldown_cmark::TagEnd::BlockQuote
                    | pulldown_cmark::TagEnd::HtmlBlock
                    | pulldown_cmark::TagEnd::Item
                    | pulldown_cmark::TagEnd::TableHead
                    | pulldown_cmark::TagEnd::TableRow
                    | pulldown_cmark::TagEnd::TableCell
                    | pulldown_cmark::TagEnd::Image,
                ) => DataAnnotation::new_interpreted_markup(content, "\n".into()),
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(_)) => {
                    in_code_block += 1;
                    DataAnnotation::new_interpreted_markup(content, "\n\n".to_owned())
                }
                pulldown_cmark::Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
                    in_code_block = in_code_block.saturating_sub(1);
                    DataAnnotation::new_interpreted_markup(content, "\n\n".to_owned())
                }
                pulldown_cmark::Event::InlineHtml(i) => {
                    if i.starts_with("<code") {
                        in_code_block += 1;
                    } else if i.starts_with("</code") {
                        in_code_block = in_code_block.saturating_sub(1);
                    }
                    DataAnnotation::new_interpreted_markup(content, "0".into())
                }
                _ => DataAnnotation::new_markup(content),
            });
        }
        tokens
    }

    pub fn map_position(&self, document: &Document, offset: usize) -> Position {
        let document = &document.text;
        let mapping = self
            .ranges
            .range(..=offset)
            .last()
            .unwrap_or(self.ranges.first_key_value().unwrap());
        let mut offset = mapping.1 + (offset - mapping.0);

        // TODO figure out why
        while !document.is_char_boundary(offset) {
            offset += 1;
        }

        let line = (document[..offset].lines().count() - 1).try_into().unwrap();
        let character = document[..offset]
            .rsplit_once('\n')
            .map_or(offset, |(_, r)| r.len())
            .try_into()
            .unwrap();

        Position { line, character }
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use config::{Flag, Node, Transform};
    use serde_json::json;

    use super::*;
    #[test]
    fn parse_transform() {
        assert_eq!(Transform::from_str(r"/^\s*\* ?//m").unwrap(), Transform {
            regex: r"^\s*\* ?".into(),
            replace: String::new(),
            flags: vec![Flag::Multiline]
        });
    }
    #[test]
    fn parse_node() {
        assert_eq!(
            serde_json::from_value::<Node>(json!({
                "type": "Text",
                "query": "",
                "transform": {
                    "a": "/a//"
                }
            }))
            .unwrap(),
            Node {
                r#type: NodeType::Text,
                query: String::new(),
                transform: [(String::from("a"), vec![
                    Transform::from_str("/a//").unwrap()
                ])]
                .into_iter()
                .collect()
            }
        );
    }
}
