//! A parser for the SpiceDB schema language: definitions, relations, permissions and caveats.
//!
//! It parses what a closure needs, exactly: every relation's allowed types (with wildcards, subject relations,
//! caveats and expiration), and every permission's expression. A caveat's body is skipped (its name is kept), and
//! `use` directives are accepted. Operator precedence follows SpiceDB: `-` is parsed loosest, then `&`, then `+`. A
//! closure does not depend on precedence, because any exclusion or intersection it can reach is refused.

use crate::{AllowedType, ArrowKind, Definition, Expr, Schema};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "schema line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Sym(&'static str),
}

const SYMS: [&str; 16] = [
    "->", "{", "}", "(", ")", ":", "|", "#", "*", "+", "&", "-", ".", ",", "=", ";",
];

/// The tokens of `src`, each with its line.
fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let b = src.as_bytes();
    let (mut i, mut line, mut toks) = (0usize, 1usize, Vec::new());
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else if src[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if src[i..].starts_with("/*") {
            let end = src[i + 2..].find("*/").ok_or(ParseError {
                line,
                message: "unterminated comment".into(),
            })?;
            line += src[i..i + 2 + end].matches('\n').count();
            i += end + 4;
        } else if c == b'"' {
            // Strings appear only inside caveat bodies, which are skipped; lex them whole so a brace in one is inert.
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            if j >= b.len() {
                return Err(ParseError {
                    line,
                    message: "unterminated string".into(),
                });
            }
            toks.push((Tok::Ident(src[i..=j].to_string()), line));
            i = j + 1;
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'/') {
                i += 1;
            }
            toks.push((Tok::Ident(src[start..i].to_string()), line));
        } else if let Some(s) = SYMS.iter().find(|s| src[i..].starts_with(**s)) {
            toks.push((Tok::Sym(s), line));
            i += s.len();
        } else if c.is_ascii() {
            // Caveat expressions use operators no other part of a schema does (==, <, !, ...): keep them as
            // one-character idents so the skipping parser can step over them.
            toks.push((Tok::Ident(char::from(c).to_string()), line));
            i += 1;
        } else {
            return Err(ParseError {
                line,
                message: "a schema is ASCII outside its comments and strings".into(),
            });
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .or(self.toks.last())
            .map_or(1, |t| t.1)
    }
    fn err<T>(&self, m: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            line: self.line(),
            message: m.into(),
        })
    }
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        self.pos += 1;
        t
    }
    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }
    fn is_ident(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(x)) if x == s)
    }
    fn sym(&mut self, s: &str) -> Result<(), ParseError> {
        if self.is_sym(s) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(format!("expected `{s}`"))
        }
    }
    fn ident(&mut self) -> Result<String, ParseError> {
        if let Some(Tok::Ident(s)) = self.next() {
            Ok(s)
        } else {
            self.pos -= 1;
            self.err("expected a name")
        }
    }
    /// Skip a balanced `{ ... }` (a caveat body).
    fn skip_block(&mut self) -> Result<(), ParseError> {
        self.sym("{")?;
        let mut depth = 1;
        while depth > 0 {
            match self.next() {
                Some(Tok::Sym("{")) => depth += 1,
                Some(Tok::Sym("}")) => depth -= 1,
                Some(_) => {}
                None => return self.err("unterminated block"),
            }
        }
        Ok(())
    }
}

/// Parse a SpiceDB schema.
///
/// # Errors
///
/// A `ParseError` naming the line of the first thing it cannot read.
pub fn parse(src: &str) -> Result<Schema, ParseError> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let mut schema = Schema::default();
    while p.peek().is_some() {
        let kw = p.ident()?;
        match kw.as_str() {
            "use" => {
                p.ident()?;
            }
            "caveat" => {
                let name = p.ident()?;
                p.sym("(")?;
                let mut depth = 1;
                while depth > 0 {
                    match p.next() {
                        Some(Tok::Sym("(")) => depth += 1,
                        Some(Tok::Sym(")")) => depth -= 1,
                        Some(_) => {}
                        None => return p.err("unterminated caveat parameters"),
                    }
                }
                p.skip_block()?;
                schema.caveats.insert(name);
            }
            "definition" => {
                let d = definition(&mut p)?;
                if schema.definitions.contains_key(&d.name) {
                    return p.err(format!("type `{}` is defined twice", d.name));
                }
                schema.definitions.insert(d.name.clone(), d);
            }
            other => return p.err(format!("unexpected `{other}` at the top level")),
        }
    }
    Ok(schema)
}

fn definition(p: &mut Parser) -> Result<Definition, ParseError> {
    let name = p.ident()?;
    let mut d = Definition {
        name,
        ..Default::default()
    };
    p.sym("{")?;
    while !p.is_sym("}") {
        let kw = p.ident()?;
        let member = p.ident()?;
        if d.relations.contains_key(&member) || d.permissions.contains_key(&member) {
            return p.err(format!("`{}#{member}` is defined twice", d.name));
        }
        match kw.as_str() {
            "relation" => {
                p.sym(":")?;
                let mut types = vec![allowed(p)?];
                while p.is_sym("|") {
                    p.sym("|")?;
                    types.push(allowed(p)?);
                }
                d.relations.insert(member, types);
            }
            "permission" => {
                p.sym("=")?;
                let e = expr(p)?;
                d.permissions.insert(member, e);
            }
            other => {
                return p.err(format!(
                    "expected `relation` or `permission`, found `{other}`"
                ))
            }
        }
        if p.is_sym(";") {
            p.sym(";")?;
        }
    }
    p.sym("}")?;
    Ok(d)
}

fn allowed(p: &mut Parser) -> Result<AllowedType, ParseError> {
    let type_name = p.ident()?;
    let mut a = AllowedType {
        type_name,
        relation: None,
        wildcard: false,
        caveat: None,
        expiration: false,
    };
    if p.is_sym(":") {
        p.sym(":")?;
        p.sym("*")?;
        a.wildcard = true;
    } else if p.is_sym("#") {
        p.sym("#")?;
        a.relation = Some(p.ident()?);
    }
    if p.is_ident("with") {
        p.ident()?;
        loop {
            let t = p.ident()?;
            if t == "expiration" {
                a.expiration = true;
            } else {
                a.caveat = Some(t);
            }
            if p.is_ident("and") {
                p.ident()?;
            } else {
                break;
            }
        }
    }
    Ok(a)
}

// expr := excl ; excl := inter ('-' inter)* ; inter := union ('&' union)* ; union := term ('+' term)*
fn expr(p: &mut Parser) -> Result<Expr, ParseError> {
    let mut left = inter(p)?;
    while p.is_sym("-") {
        p.sym("-")?;
        let right = inter(p)?;
        left = Expr::Exclude(Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn inter(p: &mut Parser) -> Result<Expr, ParseError> {
    let mut xs = vec![union(p)?];
    while p.is_sym("&") {
        p.sym("&")?;
        xs.push(union(p)?);
    }
    Ok(if xs.len() == 1 {
        xs.remove(0)
    } else {
        Expr::Intersect(xs)
    })
}

fn union(p: &mut Parser) -> Result<Expr, ParseError> {
    let mut xs = vec![term(p)?];
    while p.is_sym("+") {
        p.sym("+")?;
        xs.push(term(p)?);
    }
    Ok(if xs.len() == 1 {
        xs.remove(0)
    } else {
        Expr::Union(xs)
    })
}

fn term(p: &mut Parser) -> Result<Expr, ParseError> {
    if p.is_sym("(") {
        p.sym("(")?;
        let e = expr(p)?;
        p.sym(")")?;
        return Ok(e);
    }
    let name = p.ident()?;
    if name == "nil" {
        return Ok(Expr::Nil);
    }
    if p.is_sym("->") {
        p.sym("->")?;
        let computed = p.ident()?;
        return Ok(Expr::Arrow {
            tupleset: name,
            computed,
            kind: ArrowKind::Any,
        });
    }
    if p.is_sym(".") {
        p.sym(".")?;
        let f = p.ident()?;
        let kind = match f.as_str() {
            "any" => ArrowKind::Any,
            "all" => ArrowKind::All,
            other => return p.err(format!("unknown arrow function `{other}`")),
        };
        p.sym("(")?;
        let computed = p.ident()?;
        p.sym(")")?;
        return Ok(Expr::Arrow {
            tupleset: name,
            computed,
            kind,
        });
    }
    Ok(Expr::Ref(name))
}
