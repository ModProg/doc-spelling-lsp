use std::collections::BTreeSet;
use std::time::Duration;

use cached::proc_macro::cached;
use futures::{StreamExt, TryStreamExt};
use languagetool_rust::CheckRequest;
use languagetool_rust::check::DataAnnotation;
use log::{debug, error};
use lsp_types::{Diagnostic, DiagnosticSeverity};
use non_exhaustive::non_exhaustive;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::config::Config;
use crate::parsing::{Document, Segment};
use crate::state::State;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub missspelled: Option<String>,
    pub replacements: Vec<String>,
    pub rule: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn diagnose(
    document: &Document,
    ltex_client: &languagetool_rust::ServerClient,
    state: &State,
    config: &Config,
) -> anyhow::Result<Vec<Diagnostic>> {
    let segments = document.spell_checkable(&config.parsing);
    // let mut current = 0;
    // // First collect all the ranges that represent comment content
    // let doc_comments = ra_ap_rustc_lexer::tokenize(document)
    //     .filter_map(|RustToken { kind, len }| {
    //         let start = current as usize;
    //         let end = current + len;
    //         current = end;
    //         let end = end as usize;
    //         match kind {
    //             RustTokenKind::LineComment {
    //                 doc_style: Some(DocStyle::Inner),
    //             } => Some(Token::Inner(
    //                 (start + 3 + usize::from(document[3.min(end)..].starts_with('
    // '))).min(end)                     ..end,
    //             )),
    //             RustTokenKind::LineComment {
    //                 doc_style: Some(DocStyle::Outer),
    //             } => Some(Token::Outer(
    //                 (start + 3 + usize::from(document[3.min(end)..].starts_with('
    // '))).min(end)                     ..end,
    //             )),
    //             RustTokenKind::BlockComment {
    //                 doc_style: Some(DocStyle::Inner | DocStyle::Outer),
    //                 ..
    //             } => todo!("parse block comments"),
    //             RustTokenKind::Whitespace => None,
    //             _ => Some(Token::Break),
    //         }
    //     })
    //     .fold(vec![], {
    //         let mut last = Token::Break;
    //         move |mut b, c| {
    //             let (current, range) = match (&last, c.clone()) {
    //                 (Token::Inner(_), Token::Inner(range))
    //                 | (Token::Outer(_), Token::Outer(range)) =>
    // (b.last_mut().unwrap(), range),                 (_, Token::Inner(range) |
    // Token::Outer(range)) => {                     b.push(Comment::default());
    //                     (b.last_mut().unwrap(), range)
    //                 }
    //                 _ => {
    //                     last = c;
    //                     return b;
    //                 }
    //             };

    //             current.push(document, range);
    //             last = c;
    //             b
    //         }
    //     });

    futures::stream::iter(segments)
        .map(|c| diagnose_segment(c, document, ltex_client, state))
        .buffered(10)
        .try_fold(Vec::new(), |mut b, i| async move {
            b.extend_from_slice(&i);
            Ok(b)
        })
        .await
}

async fn diagnose_segment(
    segment: Segment,
    document: &Document,
    ltex_client: &languagetool_rust::ServerClient,
    state: &State,
) -> anyhow::Result<Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    for result in check_request(ltex_client, segment.tag_markup(), &state.disabled_rules, &state.language).await {
        const MISSPELLING: &str = "misspelling";
        let word = segment
            .text
            .get(result.offset..result.offset + result.length)
            .unwrap_or_else(|| {
                error!("invalid offset in {result:?}");
                ""
            });

        if result.rule.issue_type == MISSPELLING && state.dictionary.contains(word) {
            debug!("ignoring word in dictionary: `{word}`");
            continue;
        }
        // TODO error? because offset is external
        let start = segment.map_position(document, result.offset);
        let end = segment.map_position(document, result.offset + result.length);

        // TODO unicode :D
        // TODO code actions
        #[allow(clippy::cast_possible_truncation)]
        diagnostics.push(Diagnostic {
            range: lsp_types::Range { start, end },
            severity: Some(DiagnosticSeverity::INFORMATION),
            code: None,
            code_description: None,
            source: Some("ltex".into()),
            message: result.message,
            data: Some(
                serde_json::to_value(Meta {
                    replacements: result
                        .replacements
                        .into_iter()
                        .take(10)
                        .map(|r| r.value)
                        .collect(),
                    missspelled: (result.rule.issue_type == MISSPELLING).then(|| word.to_owned()),
                    rule: (result.rule.issue_type != MISSPELLING).then_some(result.rule.id),
                })
                .unwrap(),
            ),
            ..Default::default()
        });
    }

    Ok(diagnostics)
}

#[cached(
    size = 1000,
    key = "(Vec<DataAnnotation>, BTreeSet<String>, String)",
    convert = "{(data.clone(), disabled_rules.clone(), language.into())}"
)]
async fn check_request(
    ltex_client: &languagetool_rust::ServerClient,
    data: Vec<DataAnnotation>,
    disabled_rules: &BTreeSet<String>,
    language: &str
) -> Vec<languagetool_rust::check::Match> {
    let mut tries = 0;
    let results = loop {
        match ltex_client
            .check(&non_exhaustive!(CheckRequest {
                data: Some(non_exhaustive!(languagetool_rust::check::Data {
                    annotation: data.clone()
                })),
                language: language.into(),
                disabled_rules: Some(
                    disabled_rules
                        .iter()
                        .map(ToString::to_string)
                        .chain(["WHITESPACE_RULE".into(), "CONSECUTIVE_SPACES".into()])
                        .collect()
                ),
                ..CheckRequest::default()
            }))
            .await
        {
            Ok(results) => break results,
            Err(e) => {
                if tries > 10 {
                    error!("unable to spell check, skipping: {e}");
                    return Vec::new();
                }
                tries += 1;
                sleep(Duration::from_secs(1)).await;
            }
        }
    };

    results.matches
}
