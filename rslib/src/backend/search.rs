// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

use itertools::Itertools;
use std::convert::{TryFrom, TryInto};

use super::Backend;
use crate::{
    backend_proto as pb,
    backend_proto::{
        sort_order::builtin::Kind as SortKindProto, sort_order::Value as SortOrderProto,
    },
    config::SortKind,
    prelude::*,
    search::{
        browser, concatenate_searches, parse_search, replace_search_node, write_nodes,
        BoolSeparator, Node, PropertyKind, RatingKind, SearchNode, SortMode, StateKind,
        TemplateKind,
    },
    text::escape_anki_wildcards,
};
pub(super) use pb::search_service::Service as SearchService;

impl SearchService for Backend {
    fn build_search_string(&self, input: pb::SearchNode) -> Result<pb::String> {
        let node: Node = input.try_into()?;
        Ok(write_nodes(&node.into_node_list()).into())
    }

    fn search_cards(&self, input: pb::SearchCardsIn) -> Result<pb::SearchCardsOut> {
        self.with_col(|col| {
            let order = input.order.unwrap_or_default().value.into();
            let cids = col.search_cards(&input.search, order)?;
            Ok(pb::SearchCardsOut {
                card_ids: cids.into_iter().map(|v| v.0).collect(),
            })
        })
    }

    fn search_notes(&self, input: pb::SearchNotesIn) -> Result<pb::SearchNotesOut> {
        self.with_col(|col| {
            let nids = col.search_notes(&input.search)?;
            Ok(pb::SearchNotesOut {
                note_ids: nids.into_iter().map(|v| v.0).collect(),
            })
        })
    }

    fn join_search_nodes(&self, input: pb::JoinSearchNodesIn) -> Result<pb::String> {
        let sep = input.joiner().into();
        let existing_nodes = {
            let node: Node = input.existing_node.unwrap_or_default().try_into()?;
            node.into_node_list()
        };
        let additional_node = input.additional_node.unwrap_or_default().try_into()?;
        Ok(concatenate_searches(sep, existing_nodes, additional_node).into())
    }

    fn replace_search_node(&self, input: pb::ReplaceSearchNodeIn) -> Result<pb::String> {
        let existing = {
            let node = input.existing_node.unwrap_or_default().try_into()?;
            if let Node::Group(nodes) = node {
                nodes
            } else {
                vec![node]
            }
        };
        let replacement = input.replacement_node.unwrap_or_default().try_into()?;
        Ok(replace_search_node(existing, replacement).into())
    }

    fn find_and_replace(&self, input: pb::FindAndReplaceIn) -> Result<pb::OpChangesWithCount> {
        let mut search = if input.regex {
            input.search
        } else {
            regex::escape(&input.search)
        };
        if !input.match_case {
            search = format!("(?i){}", search);
        }
        let nids = input.nids.into_iter().map(NoteID).collect();
        let field_name = if input.field_name.is_empty() {
            None
        } else {
            Some(input.field_name)
        };
        let repl = input.replacement;
        self.with_col(|col| {
            col.find_and_replace(nids, &search, &repl, field_name)
                .map(Into::into)
        })
    }

    fn browser_row_for_card(&self, input: pb::CardId) -> Result<pb::BrowserRow> {
        self.with_col(|col| col.browser_row_for_card(input.cid.into()).map(Into::into))
    }
}

impl TryFrom<pb::SearchNode> for Node {
    type Error = AnkiError;

    fn try_from(msg: pb::SearchNode) -> std::result::Result<Self, Self::Error> {
        use pb::search_node::group::Joiner;
        use pb::search_node::Filter;
        use pb::search_node::Flag;
        Ok(if let Some(filter) = msg.filter {
            match filter {
                Filter::Tag(s) => Node::Search(SearchNode::Tag(escape_anki_wildcards(&s))),
                Filter::Deck(s) => Node::Search(SearchNode::Deck(if s == "*" {
                    s
                } else {
                    escape_anki_wildcards(&s)
                })),
                Filter::Note(s) => Node::Search(SearchNode::NoteType(escape_anki_wildcards(&s))),
                Filter::Template(u) => {
                    Node::Search(SearchNode::CardTemplate(TemplateKind::Ordinal(u as u16)))
                }
                Filter::Nid(nid) => Node::Search(SearchNode::NoteIDs(nid.to_string())),
                Filter::Nids(nids) => Node::Search(SearchNode::NoteIDs(nids.into_id_string())),
                Filter::Dupe(dupe) => Node::Search(SearchNode::Duplicates {
                    note_type_id: dupe.notetype_id.into(),
                    text: dupe.first_field,
                }),
                Filter::FieldName(s) => Node::Search(SearchNode::SingleField {
                    field: escape_anki_wildcards(&s),
                    text: "*".to_string(),
                    is_re: false,
                }),
                Filter::Rated(rated) => Node::Search(SearchNode::Rated {
                    days: rated.days,
                    ease: rated.rating().into(),
                }),
                Filter::AddedInDays(u) => Node::Search(SearchNode::AddedInDays(u)),
                Filter::DueInDays(i) => Node::Search(SearchNode::Property {
                    operator: "<=".to_string(),
                    kind: PropertyKind::Due(i),
                }),
                Filter::DueOnDay(i) => Node::Search(SearchNode::Property {
                    operator: "=".to_string(),
                    kind: PropertyKind::Due(i),
                }),
                Filter::EditedInDays(u) => Node::Search(SearchNode::EditedInDays(u)),
                Filter::CardState(state) => Node::Search(SearchNode::State(
                    pb::search_node::CardState::from_i32(state)
                        .unwrap_or_default()
                        .into(),
                )),
                Filter::Flag(flag) => match Flag::from_i32(flag).unwrap_or(Flag::Any) {
                    Flag::None => Node::Search(SearchNode::Flag(0)),
                    Flag::Any => Node::Not(Box::new(Node::Search(SearchNode::Flag(0)))),
                    Flag::Red => Node::Search(SearchNode::Flag(1)),
                    Flag::Orange => Node::Search(SearchNode::Flag(2)),
                    Flag::Green => Node::Search(SearchNode::Flag(3)),
                    Flag::Blue => Node::Search(SearchNode::Flag(4)),
                },
                Filter::Negated(term) => Node::try_from(*term)?.negated(),
                Filter::Group(mut group) => {
                    match group.nodes.len() {
                        0 => return Err(AnkiError::invalid_input("empty group")),
                        // a group of 1 doesn't need to be a group
                        1 => group.nodes.pop().unwrap().try_into()?,
                        // 2+ nodes
                        _ => {
                            let joiner = match group.joiner() {
                                Joiner::And => Node::And,
                                Joiner::Or => Node::Or,
                            };
                            let parsed: Vec<_> = group
                                .nodes
                                .into_iter()
                                .map(TryFrom::try_from)
                                .collect::<Result<_>>()?;
                            let joined = parsed.into_iter().intersperse(joiner).collect();
                            Node::Group(joined)
                        }
                    }
                }
                Filter::ParsableText(text) => {
                    let mut nodes = parse_search(&text)?;
                    if nodes.len() == 1 {
                        nodes.pop().unwrap()
                    } else {
                        Node::Group(nodes)
                    }
                }
            }
        } else {
            Node::Search(SearchNode::WholeCollection)
        })
    }
}

impl From<pb::search_node::group::Joiner> for BoolSeparator {
    fn from(sep: pb::search_node::group::Joiner) -> Self {
        match sep {
            pb::search_node::group::Joiner::And => BoolSeparator::And,
            pb::search_node::group::Joiner::Or => BoolSeparator::Or,
        }
    }
}

impl From<pb::search_node::Rating> for RatingKind {
    fn from(r: pb::search_node::Rating) -> Self {
        match r {
            pb::search_node::Rating::Again => RatingKind::AnswerButton(1),
            pb::search_node::Rating::Hard => RatingKind::AnswerButton(2),
            pb::search_node::Rating::Good => RatingKind::AnswerButton(3),
            pb::search_node::Rating::Easy => RatingKind::AnswerButton(4),
            pb::search_node::Rating::Any => RatingKind::AnyAnswerButton,
            pb::search_node::Rating::ByReschedule => RatingKind::ManualReschedule,
        }
    }
}

impl From<pb::search_node::CardState> for StateKind {
    fn from(k: pb::search_node::CardState) -> Self {
        match k {
            pb::search_node::CardState::New => StateKind::New,
            pb::search_node::CardState::Learn => StateKind::Learning,
            pb::search_node::CardState::Review => StateKind::Review,
            pb::search_node::CardState::Due => StateKind::Due,
            pb::search_node::CardState::Suspended => StateKind::Suspended,
            pb::search_node::CardState::Buried => StateKind::Buried,
        }
    }
}

impl pb::search_node::IdList {
    fn into_id_string(self) -> String {
        self.ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl From<SortKindProto> for SortKind {
    fn from(kind: SortKindProto) -> Self {
        match kind {
            SortKindProto::NoteCreation => SortKind::NoteCreation,
            SortKindProto::NoteMod => SortKind::NoteMod,
            SortKindProto::NoteField => SortKind::NoteField,
            SortKindProto::NoteTags => SortKind::NoteTags,
            SortKindProto::NoteType => SortKind::NoteType,
            SortKindProto::CardMod => SortKind::CardMod,
            SortKindProto::CardReps => SortKind::CardReps,
            SortKindProto::CardDue => SortKind::CardDue,
            SortKindProto::CardEase => SortKind::CardEase,
            SortKindProto::CardLapses => SortKind::CardLapses,
            SortKindProto::CardInterval => SortKind::CardInterval,
            SortKindProto::CardDeck => SortKind::CardDeck,
            SortKindProto::CardTemplate => SortKind::CardTemplate,
        }
    }
}

impl From<Option<SortOrderProto>> for SortMode {
    fn from(order: Option<SortOrderProto>) -> Self {
        use pb::sort_order::Value as V;
        match order.unwrap_or(V::FromConfig(pb::Empty {})) {
            V::None(_) => SortMode::NoOrder,
            V::Custom(s) => SortMode::Custom(s),
            V::FromConfig(_) => SortMode::FromConfig,
            V::Builtin(b) => SortMode::Builtin {
                kind: b.kind().into(),
                reverse: b.reverse,
            },
        }
    }
}

impl From<browser::Row> for pb::BrowserRow {
    fn from(row: browser::Row) -> Self {
        pb::BrowserRow {
            cells: row.cells.into_iter().map(Into::into).collect(),
            color: row.color.into(),
            font_name: row.font.name,
            font_size: row.font.size,
        }
    }
}

impl From<browser::Cell> for pb::browser_row::Cell {
    fn from(cell: browser::Cell) -> Self {
        pb::browser_row::Cell {
            text: cell.text,
            is_rtl: cell.is_rtl,
        }
    }
}

impl From<browser::RowColor> for i32 {
    fn from(color: browser::RowColor) -> Self {
        match color {
            browser::RowColor::Default => pb::browser_row::Color::Default as i32,
            browser::RowColor::Marked => pb::browser_row::Color::Marked as i32,
            browser::RowColor::Suspended => pb::browser_row::Color::Suspended as i32,
            browser::RowColor::FlagRed => pb::browser_row::Color::FlagRed as i32,
            browser::RowColor::FlagOrange => pb::browser_row::Color::FlagOrange as i32,
            browser::RowColor::FlagGreen => pb::browser_row::Color::FlagGreen as i32,
            browser::RowColor::FlagBlue => pb::browser_row::Color::FlagBlue as i32,
        }
    }
}
