use arsy_kernel::domain::StateVersion;
use sha2::{Digest, Sha256};
use std::{fmt, ops::Range};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditIntent {
    ExactText,
    Structure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    Deferred,
    Incremental,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxNode {
    pub identity: usize,
    pub kind: String,
    pub bytes: Range<usize>,
    pub source_revision: StateVersion,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SyntaxError {
    SourceTooLarge,
    InvalidRange,
    StaleRevision,
    StaleSyntax,
    NodeNotFound,
    ParseFailed,
    Grammar(String),
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceTooLarge => formatter.write_str("source exceeds syntax limit"),
            Self::InvalidRange => formatter.write_str("edit range is outside the source"),
            Self::StaleRevision => formatter.write_str("source revision does not match"),
            Self::StaleSyntax => formatter.write_str("syntax was deferred and must be reparsed"),
            Self::NodeNotFound => formatter.write_str("syntax node no longer exists"),
            Self::ParseFailed => formatter.write_str("tree-sitter did not produce a tree"),
            Self::Grammar(error) => {
                write!(formatter, "pinned Rust grammar is incompatible: {error}")
            }
        }
    }
}

impl std::error::Error for SyntaxError {}

pub struct RustSyntax {
    parser: Parser,
    tree: Tree,
    source: Vec<u8>,
    revision: StateVersion,
    tree_revision: StateVersion,
}

impl RustSyntax {
    pub fn new(source: impl Into<Vec<u8>>) -> Result<Self, SyntaxError> {
        let source = source.into();
        if source.len() > MAX_SOURCE_BYTES {
            return Err(SyntaxError::SourceTooLarge);
        }
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|error| SyntaxError::Grammar(error.to_string()))?;
        let tree = parser
            .parse(&source, None)
            .ok_or(SyntaxError::ParseFailed)?;
        let revision = revision(&source);
        Ok(Self {
            parser,
            tree,
            source,
            revision,
            tree_revision: revision,
        })
    }

    pub const fn revision(&self) -> StateVersion {
        self.revision
    }

    pub fn nodes(&self, kind: &str) -> Result<Vec<SyntaxNode>, SyntaxError> {
        self.require_fresh_tree()?;
        let mut matches = Vec::new();
        collect(self.tree.root_node(), kind, self.revision, &mut matches);
        Ok(matches)
    }

    pub fn imports(&self) -> Result<Vec<SyntaxNode>, SyntaxError> {
        self.nodes("use_declaration")
    }

    pub fn apply_edit(
        &mut self,
        expected: StateVersion,
        bytes: Range<usize>,
        replacement: &[u8],
        intent: EditIntent,
    ) -> Result<ParseOutcome, SyntaxError> {
        if expected != self.revision {
            return Err(SyntaxError::StaleRevision);
        }
        if bytes.start > bytes.end || bytes.end > self.source.len() {
            return Err(SyntaxError::InvalidRange);
        }
        let new_len = self
            .source
            .len()
            .checked_sub(bytes.len())
            .and_then(|length| length.checked_add(replacement.len()))
            .filter(|length| *length <= MAX_SOURCE_BYTES)
            .ok_or(SyntaxError::SourceTooLarge)?;
        let start_position = point_at(&self.source, bytes.start);
        let old_end_position = point_at(&self.source, bytes.end);
        self.source
            .splice(bytes.clone(), replacement.iter().copied());
        debug_assert_eq!(self.source.len(), new_len);
        let new_end_byte = bytes.start + replacement.len();
        self.tree.edit(&InputEdit {
            start_byte: bytes.start,
            old_end_byte: bytes.end,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position: point_at(&self.source, new_end_byte),
        });
        self.revision = revision(&self.source);
        if intent == EditIntent::ExactText {
            return Ok(ParseOutcome::Deferred);
        }
        self.reparse()
    }

    pub fn apply_node_edit(
        &mut self,
        node: &SyntaxNode,
        replacement: &[u8],
    ) -> Result<ParseOutcome, SyntaxError> {
        self.require_fresh_tree()?;
        if node.source_revision != self.revision {
            return Err(SyntaxError::StaleRevision);
        }
        let current =
            find(self.tree.root_node(), node.identity).ok_or(SyntaxError::NodeNotFound)?;
        if current.kind() != node.kind || current.byte_range() != node.bytes {
            return Err(SyntaxError::NodeNotFound);
        }
        self.apply_edit(
            node.source_revision,
            node.bytes.clone(),
            replacement,
            EditIntent::Structure,
        )
    }

    pub fn reparse(&mut self) -> Result<ParseOutcome, SyntaxError> {
        self.tree = self
            .parser
            .parse(&self.source, Some(&self.tree))
            .ok_or(SyntaxError::ParseFailed)?;
        self.tree_revision = self.revision;
        Ok(ParseOutcome::Incremental)
    }

    fn require_fresh_tree(&self) -> Result<(), SyntaxError> {
        if self.tree_revision == self.revision {
            Ok(())
        } else {
            Err(SyntaxError::StaleSyntax)
        }
    }
}

fn collect(node: Node<'_>, kind: &str, revision: StateVersion, matches: &mut Vec<SyntaxNode>) {
    if matches.len() == MAX_RESULTS {
        return;
    }
    if node.kind() == kind {
        matches.push(SyntaxNode {
            identity: node.id(),
            kind: node.kind().to_owned(),
            bytes: node.byte_range(),
            source_revision: revision,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, kind, revision, matches);
        if matches.len() == MAX_RESULTS {
            break;
        }
    }
}

fn find(node: Node<'_>, identity: usize) -> Option<Node<'_>> {
    if node.id() == identity {
        return Some(node);
    }
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .find_map(|child| find(child, identity));
    found
}

fn point_at(source: &[u8], byte: usize) -> Point {
    let before = &source[..byte];
    let row = before.iter().filter(|value| **value == b'\n').count();
    let column = before
        .iter()
        .rposition(|value| *value == b'\n')
        .map_or(byte, |newline| before.len() - newline - 1);
    Point::new(row, column)
}

fn revision(source: &[u8]) -> StateVersion {
    StateVersion::from_digest(Sha256::digest(source).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_bound_queries_incremental_edits_and_text_bypass() {
        let mut syntax =
            RustSyntax::new("use std::io;\nfn main() { println!(\"old\"); }\n").unwrap();
        let import = syntax.imports().unwrap().pop().unwrap();
        assert_eq!(import.kind, "use_declaration");

        assert_eq!(
            syntax.apply_node_edit(&import, b"use std::fmt;").unwrap(),
            ParseOutcome::Incremental
        );
        assert_eq!(syntax.imports().unwrap().len(), 1);
        assert!(matches!(
            syntax.apply_node_edit(&import, b"use std::fs;"),
            Err(SyntaxError::StaleRevision)
        ));

        let current = syntax.revision();
        let offset = syntax
            .source
            .windows(3)
            .position(|window| window == b"old")
            .unwrap();
        assert_eq!(
            syntax
                .apply_edit(current, offset..offset + 3, b"new", EditIntent::ExactText)
                .unwrap(),
            ParseOutcome::Deferred
        );
        assert_eq!(syntax.imports(), Err(SyntaxError::StaleSyntax));
        assert_eq!(syntax.reparse().unwrap(), ParseOutcome::Incremental);
        assert_eq!(syntax.nodes("function_item").unwrap().len(), 1);
    }
}
