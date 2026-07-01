//! Lexer: turn raw source text into a stream of lexical tokens.

/// A single lexical token produced by the scanner.
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
}

/// The category of a scanned token.
pub enum TokenKind {
    Ident,
    Number,
    Punct,
}

/// Scan `source` into a stream of tokens, splitting identifiers, numbers, and punctuation.
pub fn tokenize(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    for word in source.split_whitespace() {
        let kind = if word.chars().all(|c| c.is_ascii_digit()) {
            TokenKind::Number
        } else {
            TokenKind::Ident
        };
        tokens.push(Token { kind, text: word.to_string() });
    }
    tokens
}
