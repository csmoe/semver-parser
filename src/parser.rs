//! Recursive-descent parser for semver ranges.
//!
//! The parsers is divided into a set of functions, each responsible for parsing a subset of the
//! grammar.
//!
//! # Examples
//!
//! ```rust
//! use semver_parser::parser::Parser;
//! use semver_parser::range::{ CompatibleOp, Op };
//!
//! let mut p = Parser::new("^1").expect("a broken parser");
//!
//! assert_eq!(Ok(Op::Compatible(CompatibleOp::Caret)), p.op());
//! assert_eq!(Ok(Some(1)), p.component());
//! ```
//!
//! Example parsing a range:
//!
//! ```rust
//! use semver_parser::parser::Parser;
//! use semver_parser::range::{CompatibleOp, Op, Predicate};
//!
//! let mut p = Parser::new("^1.0").expect("a broken parser");
//!
//! assert_eq!(Ok(Some(Predicate {
//!     op: Op::Compatible(CompatibleOp::Caret),
//!     major: 1,
//!     minor: Some(0),
//!     patch: None,
//!     pre: vec![],
//! })), p.predicate());
//!
//! let mut p = Parser::new("^*").expect("a broken parser");
//!
//! assert_eq!(Ok(None), p.predicate());
//! ```

use lexer::{self, Lexer, Token};
use self::Error::*;
use range::{Predicate, Op, CompatibleOp, VersionReq, WildcardVersion};
use comparator::Comparator;
use version::{Version, Identifier};
use std::mem;
use std::fmt;

/// Evaluate if parser contains the given pattern as a separator, surrounded by whitespace.
macro_rules! has_ws_separator {
    ($slf:expr, $pat:pat) => {{
        $slf.skip_whitespace()?;

        match $slf.peek() {
            $pat => {
                // pop the separator.
                $slf.pop()?;
                // strip suffixing whitespace.
                $slf.skip_whitespace()?;
                true
            },
            _ => false,
        }
    }}
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Error<'input> {
    /// Needed more tokens for parsing, but none are available.
    UnexpectedEnd,
    /// Unexpected token.
    UnexpectedToken(Token<'input>),
    /// An error occurred in the lexer.
    Lexer(lexer::Error),
    /// More input available.
    MoreInput(Vec<Token<'input>>),
    /// Encountered empty predicate in a set of predicates.
    EmptyPredicate,
    /// Encountered an empty range.
    EmptyRange,
}

impl<'input> From<lexer::Error> for Error<'input> {
    fn from(value: lexer::Error) -> Self {
        Error::Lexer(value)
    }
}

impl<'input> fmt::Display for Error<'input> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match *self {
            UnexpectedEnd => write!(fmt, "expected more input"),
            UnexpectedToken(ref token) => write!(fmt, "encountered unexpected token: {:?}", token),
            Lexer(ref error) => write!(fmt, "lexer error: {:?}", error),
            MoreInput(ref tokens) => write!(fmt, "expected end of input, but got: {:?}", tokens),
            EmptyPredicate => write!(fmt, "encountered empty predicate"),
            EmptyRange => write!(fmt, "encountered empty range"),
        }
    }
}

/// impl for backwards compatibility.
impl<'input> From<Error<'input>> for String {
    fn from(value: Error<'input>) -> Self {
        value.to_string()
    }
}

/// A recursive-descent parser for parsing version requirements.
pub struct Parser<'input> {
    /// Source of token.
    lexer: Lexer<'input>,
    /// Lookaehead.
    c1: Option<Token<'input>>,
}

impl<'input> Parser<'input> {
    /// Construct a new parser for the given input.
    pub fn new(input: &'input str) -> Result<Parser<'input>, Error<'input>> {
        let mut lexer = Lexer::new(input);

        let c1 = if let Some(c1) = lexer.next() {
            Some(c1?)
        } else {
            None
        };

        Ok(Parser {
            lexer: lexer,
            c1: c1,
        })
    }

    /// Pop one token.
    #[inline(always)]
    fn pop(&mut self) -> Result<Token<'input>, Error<'input>> {
        let c1 = if let Some(c1) = self.lexer.next() {
            Some(c1?)
        } else {
            None
        };

        mem::replace(&mut self.c1, c1).ok_or_else(|| UnexpectedEnd)
    }

    /// Peek one token.
    #[inline(always)]
    fn peek(&mut self) -> Option<&Token<'input>> {
        self.c1.as_ref()
    }

    /// Skip whitespace if present.
    fn skip_whitespace(&mut self) -> Result<(), Error<'input>> {
        match self.peek() {
            Some(&Token::Whitespace(_, _)) => self.pop().map(|_| ()),
            _ => Ok(()),
        }
    }

    /// Parse an optional comma separator, then if that is present a predicate.
    pub fn comma_predicate(&mut self) -> Result<Option<Predicate>, Error<'input>> {
        if !has_ws_separator!(self, Some(&Token::Comma)) {
            return Ok(None);
        }

        if let Some(predicate) = self.predicate()? {
            Ok(Some(predicate))
        } else {
            Err(EmptyPredicate)
        }
    }

    /// Parse an optional or separator `||`, then if that is present a range.
    fn or_range(&mut self) -> Result<Option<VersionReq>, Error<'input>> {
        if !has_ws_separator!(self, Some(&Token::Or)) {
            return Ok(None);
        }

        Ok(Some(self.range()?))
    }

    /// Parse a single component.
    ///
    /// Returns `None` if the component is a wildcard.
    pub fn component(&mut self) -> Result<Option<u64>, Error<'input>> {
        match self.pop()? {
            Token::Numeric(number) => Ok(Some(number)),
            ref t if t.is_wildcard() => Ok(None),
            tok => Err(UnexpectedToken(tok)),
        }
    }

    /// Parse a single numeric.
    pub fn numeric(&mut self) -> Result<u64, Error<'input>> {
        match self.pop()? {
            Token::Numeric(number) => Ok(number),
            tok => Err(UnexpectedToken(tok)),
        }
    }

    /// Optionally parse a dot, then a component.
    ///
    /// The second component of the tuple indicates if a wildcard has been encountered, and is
    /// always `false` if the first component is `Some`.
    ///
    /// If a dot is not encountered, `(None, false)` is returned.
    ///
    /// If a wildcard is encountered, `(None, true)` is returned.
    pub fn dot_component(&mut self) -> Result<(Option<u64>, bool), Error<'input>> {
        match self.peek() {
            Some(&Token::Dot) => {}
            _ => return Ok((None, false)),
        }

        // pop the peeked dot.
        self.pop()?;
        self.component().map(|n| (n, n.is_none()))
    }

    /// Parse a dot, then a numeric.
    pub fn dot_numeric(&mut self) -> Result<u64, Error<'input>> {
        match self.pop()? {
            Token::Dot => {}
            tok => return Err(UnexpectedToken(tok)),
        }

        self.numeric()
    }

    /// Parse an string identifier.
    ///
    /// Like, `foo`, or `bar`.
    pub fn identifier(&mut self) -> Result<Identifier, Error<'input>> {
        let identifier = match self.pop()? {
            Token::AlphaNumeric(identifier) => {
                // TODO: Borrow?
                Identifier::AlphaNumeric(identifier.to_string())
            }
            Token::Numeric(n) => Identifier::Numeric(n),
            tok => return Err(UnexpectedToken(tok)),
        };

        Ok(identifier)
    }

    /// Parse all pre-release identifiers, separated by dots.
    ///
    /// Like, `abcdef.1234`.
    fn pre(&mut self) -> Result<Vec<Identifier>, Error<'input>> {
        match self.peek() {
            Some(&Token::Hyphen) => {}
            _ => return Ok(vec![]),
        }

        // pop the peeked hyphen.
        self.pop()?;
        self.parts()
    }

    /// Parse a dot-separated set of identifiers.
    fn parts(&mut self) -> Result<Vec<Identifier>, Error<'input>> {
        let mut parts = Vec::new();

        parts.push(self.identifier()?);

        loop {
            match self.peek() {
                Some(&Token::Dot) => {}
                _ => break,
            }

            // pop the peeked hyphen.
            self.pop()?;

            parts.push(self.identifier()?);
        }

        Ok(parts)
    }

    /// Parse optional build metadata.
    ///
    /// Like, `` (empty), or `+abcdef`.
    fn plus_build_metadata(&mut self) -> Result<Vec<Identifier>, Error<'input>> {
        match self.peek() {
            Some(&Token::Plus) => {}
            _ => return Ok(vec![]),
        }

        // pop the plus.
        self.pop()?;
        self.parts()
    }

    /// Optionally parse a single operator.
    ///
    /// Like, `~`, or `^`.
    pub fn op(&mut self) -> Result<Op, Error<'input>> {
        use self::Token::*;

        let op = match self.peek() {
            Some(&Eq) => Op::Ex,
            Some(&Gt) => Op::Gt,
            Some(&GtEq) => Op::GtEq,
            Some(&Lt) => Op::Lt,
            Some(&LtEq) => Op::LtEq,
            Some(&Tilde) => Op::Tilde,
            Some(&Caret) => Op::Compatible(CompatibleOp::Caret),
            // default op
            _ => return Ok(Op::Compatible(CompatibleOp::Default_)),
        };

        // remove the matched token.
        self.pop()?;
        self.skip_whitespace()?;
        Ok(op)
    }

    /// Parse a single predicate.
    ///
    /// Like, `^1`, or `>=2.0.0`.
    pub fn predicate(&mut self) -> Result<Option<Predicate>, Error<'input>> {
        // empty predicate, treated the same as wildcard.
        if self.peek().is_none() {
            return Ok(None);
        }

        let mut op = self.op()?;

        let major = match self.component()? {
            Some(major) => major,
            None => return Ok(None),
        };

        let (minor, minor_wildcard) = self.dot_component()?;
        let (patch, patch_wildcard) = self.dot_component()?;
        let pre = self.pre()?;

        // TODO: avoid illegal combinations, like `1.*.0`.
        if minor_wildcard {
            op = Op::Wildcard(WildcardVersion::Minor);
        }

        if patch_wildcard {
            op = Op::Wildcard(WildcardVersion::Patch);
        }

        // ignore build metadata
        self.plus_build_metadata()?;

        Ok(Some(Predicate {
            op: op,
            major: major,
            minor: minor,
            patch: patch,
            pre: pre,
        }))
    }

    /// Parse a single range.
    ///
    /// Like, `^1.0` or `>=3.0.0, <4.0.0`.
    pub fn range(&mut self) -> Result<VersionReq, Error<'input>> {
        let mut predicates = Vec::new();

        if let Some(predicate) = self.predicate()? {
            predicates.push(predicate);

            while let Some(next) = self.comma_predicate()? {
                predicates.push(next);
            }
        }

        Ok(VersionReq { predicates: predicates })
    }

    /// Parse a comparator.
    ///
    /// Like, `1.0 || 2.0` or `^1 || >=3.0.0, <4.0.0`.
    pub fn comparator(&mut self) -> Result<Comparator, Error<'input>> {
        let mut ranges = Vec::new();
        ranges.push(self.range()?);

        while let Some(next) = self.or_range()? {
            ranges.push(next);
        }

        Ok(Comparator { ranges: ranges })
    }

    /// Parse a version.
    ///
    /// Like, `1.0.0` or `3.0.0-beta.1`.
    pub fn version(&mut self) -> Result<Version, Error<'input>> {
        self.skip_whitespace()?;

        let major = self.numeric()?;
        let minor = self.dot_numeric()?;
        let patch = self.dot_numeric()?;
        let pre = self.pre()?;
        let build = self.plus_build_metadata()?;

        self.skip_whitespace()?;

        Ok(Version {
            major: major,
            minor: minor,
            patch: patch,
            pre: pre,
            build: build,
        })
    }

    /// Check if we have reached the end of input.
    pub fn is_eof(&mut self) -> bool {
        self.c1.is_none()
    }

    /// Get the rest of the tokens in the parser.
    ///
    /// Useful for debugging.
    pub fn tail(&mut self) -> Result<Vec<Token<'input>>, Error<'input>> {
        let mut out = Vec::new();

        if let Some(t) = self.c1.take() {
            out.push(t);
        }

        while let Some(t) = self.lexer.next() {
            out.push(t?);
        }

        Ok(out)
    }
}
