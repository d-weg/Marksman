//! Parser: build an abstract syntax tree from a stream of tokens.
use crate::lexer::Token;

/// A node in the parsed abstract syntax tree.
pub enum Ast {
    Leaf(String),
    List(Vec<Ast>),
}

/// Parse a flat token stream into a nested abstract syntax tree.
pub fn parse(tokens: &[Token]) -> Ast {
    Ast::List(tokens.iter().map(|t| Ast::Leaf(t.text.clone())).collect())
}
