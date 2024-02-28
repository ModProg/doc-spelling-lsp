use std::collections::BTreeMap;
use std::ops::Range;
use std::time::Duration;

use languagetool_rust::check::DataAnnotation;
use languagetool_rust::CheckRequest;
use non_exhaustive::non_exhaustive;
use ra_ap_rustc_lexer::{DocStyle, Token as RustToken, TokenKind as RustTokenKind};
use tokio::time::sleep;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position};

#[derive(Clone)]
enum Token {
    Inner(Range<usize>),
    Outer(Range<usize>),
    Break,
}

#[derive(Default)]
struct Comment {
    content: String,
    ranges: BTreeMap<usize, usize>,
}

impl Comment {
    fn tag_markup(&self) -> Vec<DataAnnotation> {
        let mut parser = pulldown_cmark::Parser::new(&self.content)
            .into_offset_iter()
            .peekable();
        eprintln!("{}", self.content);
        let mut in_code_block = 0;
        let mut last = 0;
        let mut tokens = Vec::new();
        while let Some((event, mut range)) = parser.next() {
            if range.start > last {
                tokens.push(DataAnnotation::new_markup(
                    self.content[last..range.start].to_owned(),
                ));
            } else {
                range.start = range.start.max(last);
            }
            if matches!(event, pulldown_cmark::Event::Start(_)) {
                range.end = parser.peek().map_or(range.end, |e| e.1.start);
            }
            last = range.end;
            let content = self.content[range].to_owned();
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
                    in_code_block -= 1;
                    DataAnnotation::new_interpreted_markup(content, "\n\n".to_owned())
                }
                _ => DataAnnotation::new_markup(content),
            });
        }
        tokens
    }

    fn push(&mut self, document: &str, range: Range<usize>) {
        let start = self.content.len();
        self.ranges.insert(start, range.start);
        self.content.push_str(&document[range.clone()]);
        self.content.push('\n');
    }

    fn map_position(&self, document: &str, offset: usize) -> Position {
        let mapping = self
            .ranges
            .range(..=offset)
            .last()
            .unwrap_or(self.ranges.first_key_value().unwrap());
        let offset = mapping.1 + (offset - mapping.0);

        let line = (document[..offset].lines().count() - 1).try_into().unwrap();
        let character = document[..offset]
            .rsplit_once('\n')
            .map_or(offset, |(_, r)| r.len())
            .try_into()
            .unwrap();

        Position { line, character }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn diagnose(
    document: &str,
    ltex_client: &languagetool_rust::ServerClient,
) -> anyhow::Result<Vec<Diagnostic>> {
    let mut current = 0;
    // First collect all the ranges that represent comment content
    let doc_comments = ra_ap_rustc_lexer::tokenize(document)
        .filter_map(|RustToken { kind, len }| {
            let start = current as usize;
            let end = current + len;
            current = end;
            let end = end as usize;
            match kind {
                RustTokenKind::LineComment {
                    doc_style: Some(DocStyle::Inner),
                } => Some(Token::Inner(
                    (start + 3 + usize::from(document[3.min(end)..].starts_with(' '))).min(end)
                        ..end,
                )),
                RustTokenKind::LineComment {
                    doc_style: Some(DocStyle::Outer),
                } => Some(Token::Outer(
                    (start + 3 + usize::from(document[3.min(end)..].starts_with(' '))).min(end)
                        ..end,
                )),
                RustTokenKind::BlockComment {
                    doc_style: Some(DocStyle::Inner | DocStyle::Outer),
                    ..
                } => todo!("parse block comments"),
                RustTokenKind::Whitespace => None,
                _ => Some(Token::Break),
            }
        })
        .fold(vec![], {
            let mut last = Token::Break;
            move |mut b, c| {
                let (current, range) = match (&last, c.clone()) {
                    (Token::Inner(_), Token::Inner(range))
                    | (Token::Outer(_), Token::Outer(range)) => (b.last_mut().unwrap(), range),
                    (_, Token::Inner(range) | Token::Outer(range)) => {
                        b.push(Comment::default());
                        (b.last_mut().unwrap(), range)
                    }
                    _ => {
                        last = c;
                        return b;
                    }
                };

                current.push(document, range);
                last = c;
                b
            }
        });

    let mut diagnostics = Vec::new();

    for comment in doc_comments {
        let mut tries = 0;
        let results = loop {
            match ltex_client
                .check(&non_exhaustive!(CheckRequest {
                    data: Some(non_exhaustive!(languagetool_rust::check::Data {
                        annotation: comment.tag_markup()
                    })),
                    language: "en-US".into(),
                    disabled_rules: Some(vec![
                        "WHITESPACE_RULE".into(),
                        "CONSECUTIVE_SPACES".into()
                    ]),
                    ..CheckRequest::default()
                }))
                .await
            {
                Ok(results) => break results,
                Err(e) => {
                    if tries > 10 {
                        todo!("{e:?}")
                    }
                    tries += 1;
                    sleep(Duration::from_secs(1)).await;
                }
            }
        };

        for result in results.matches {
            // TODO error? because offset is external
            let start = comment.map_position(document, result.offset);
            let end = comment.map_position(document, result.offset + result.length);

            // TODO unicode :D
            // TODO code actions
            diagnostics.push(Diagnostic {
                range: tower_lsp::lsp_types::Range { start, end },
                severity: Some(DiagnosticSeverity::INFORMATION),
                code: None,
                code_description: None,
                source: Some("ltex".into()),
                message: result.message,
                data: Some(
                    serde_json::to_value(
                        result.replacements[0..result.replacements.len().min(10)].to_vec(),
                    )
                    .unwrap(),
                ),
                ..Default::default()
            });
        }
    }

    Ok(diagnostics)
}
