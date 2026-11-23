//! A SpiceDB schema, parsed, and the CLOSURE of one permission: every relation whose tuples can change who holds it.
//!
//! pg_fence projects one permission (say `container#view`) into Postgres. Two facts depend on knowing exactly which
//! relations feed it: a delivered grant on one of them must refresh a reader, and a delete on one must narrow one.
//! A relation missed here is a grant that never shows up or, worse, a revocation that never narrows. So the list is
//! derived from the schema, never typed by hand, and the derivation REFUSES every shape it cannot narrow soundly.
//!
//! The shape slice 1 accepts, for the projected type `R` and permission `p`:
//!
//! - `p` is a union (`+`) of relations and permissions on `R`;
//! - a relation on `R` whose allowed types are plain object types: `direct`;
//! - an arrow `t->p` where `t` is a relation on `R` pointing back at `R`: a self-arrow (a file under its space);
//! - an arrow `t->q` where `t` points at another type `W` and `q` is a union of plain relations on `W`: the arrow is
//!   kind `arrow`, and each relation on `W` it reaches is kind `via`, reached `through` `t`.
//!
//! - a direct or via relation may also accept a SUBJECT SET `T#r` (`group#member`), where `r` is a plain relation on
//!   `T` whose allowed types are object types: `r` on `T` is kind `set` (its tuples are memberships).
//!
//! Refused, each with its reason: exclusion (`-`, removing a tuple can grant), intersection (`&`, `.all()`), caveats,
//! expiring relationships, wildcards (`user:*`), a subject set that is itself a permission or holds further subject
//! sets (nested groups), and an arrow more than one hop away.

mod parse;

pub use parse::{parse, ParseError};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A parsed schema: its object definitions, and the names of its caveats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schema {
    pub definitions: BTreeMap<String, Definition>,
    pub caveats: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    pub relations: BTreeMap<String, Vec<AllowedType>>,
    pub permissions: BTreeMap<String, Expr>,
}

/// One allowed subject type of a relation: `user`, `group#member`, `user:*`, `user with caveat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedType {
    pub type_name: String,
    pub relation: Option<String>,
    pub wildcard: bool,
    pub caveat: Option<String>,
    pub expiration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrowKind {
    /// `t->p` or `t.any(p)`
    Any,
    /// `t.all(p)`
    All,
}

/// A permission expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Nil,
    Ref(String),
    Arrow {
        tupleset: String,
        computed: String,
        kind: ArrowKind,
    },
    Union(Vec<Expr>),
    Intersect(Vec<Expr>),
    Exclude(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Direct,
    Arrow,
    Via,
    /// A relation whose tuples are memberships of a subject set another view relation grants to.
    Set,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Direct => "direct",
            Kind::Arrow => "arrow",
            Kind::Via => "via",
            Kind::Set => "set",
        }
    }
}

/// One row of `fence.view_relation`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ViewRelation {
    pub resource_type: String,
    pub relation: String,
    pub kind: Kind,
    pub target_type: Option<String>,
    pub through: Option<String>,
}

/// Why the closure was refused. The schema is not wrong; pg_fence cannot narrow it soundly yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub at: String,
    pub reason: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.at, self.reason)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosureError {
    UnknownType(String),
    UnknownPermission(String),
    Refused(Vec<Refusal>),
}

impl fmt::Display for ClosureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClosureError::UnknownType(t) => write!(f, "the schema defines no type `{t}`"),
            ClosureError::UnknownPermission(p) => {
                write!(f, "the schema defines no permission or relation `{p}`")
            }
            ClosureError::Refused(r) => {
                write!(f, "pg_fence cannot project this permission soundly:")?;
                for x in r {
                    write!(f, "\n  - {x}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ClosureError {}

/// The relations that can change `resource_type#permission`, or every reason the shape is refused.
pub fn closure(
    schema: &Schema,
    resource_type: &str,
    permission: &str,
) -> Result<Vec<ViewRelation>, ClosureError> {
    let root = schema
        .definitions
        .get(resource_type)
        .ok_or_else(|| ClosureError::UnknownType(resource_type.to_string()))?;
    if !root.permissions.contains_key(permission) && !root.relations.contains_key(permission) {
        return Err(ClosureError::UnknownPermission(format!(
            "{resource_type}#{permission}"
        )));
    }
    let mut walk = Walk {
        schema,
        root: resource_type,
        permission,
        out: BTreeSet::new(),
        refused: vec![],
        seen: BTreeSet::new(),
    };
    walk.name(resource_type, permission, None);
    if walk.refused.is_empty() {
        Ok(walk.out.into_iter().collect())
    } else {
        walk.refused
            .sort_by(|a, b| a.at.cmp(&b.at).then(a.reason.cmp(&b.reason)));
        walk.refused.dedup();
        Err(ClosureError::Refused(walk.refused))
    }
}

struct Walk<'a> {
    schema: &'a Schema,
    root: &'a str,
    permission: &'a str,
    out: BTreeSet<ViewRelation>,
    refused: Vec<Refusal>,
    seen: BTreeSet<(String, String, Option<String>)>,
}

impl Walk<'_> {
    fn refuse(&mut self, at: String, reason: &str) {
        self.refused.push(Refusal {
            at,
            reason: reason.to_string(),
        });
    }

    /// `through` is the arrow on the root type this name was reached by, when it is on another type.
    fn name(&mut self, ty: &str, name: &str, through: Option<&str>) {
        if !self.seen.insert((
            ty.to_string(),
            name.to_string(),
            through.map(str::to_string),
        )) {
            return;
        }
        let Some(def) = self.schema.definitions.get(ty) else {
            self.refuse(
                format!("{ty}#{name}"),
                "names a type the schema does not define",
            );
            return;
        };
        if let Some(types) = def.relations.get(name) {
            self.relation(ty, name, types, through);
        } else if let Some(expr) = def.permissions.get(name) {
            let expr = expr.clone();
            self.expr(ty, name, &expr, through);
        } else {
            self.refuse(
                format!("{ty}#{name}"),
                "is neither a relation nor a permission",
            );
        }
    }

    fn relation(&mut self, ty: &str, name: &str, types: &[AllowedType], through: Option<&str>) {
        let at = format!("{ty}#{name}");
        for a in types {
            if a.caveat.is_some() {
                self.refuse(
                    at.clone(),
                    "a caveated relationship cannot be evaluated locally",
                );
            }
            if a.expiration {
                self.refuse(
                    at.clone(),
                    "an expiring relationship cannot be evaluated locally",
                );
            }
            if a.wildcard {
                self.refuse(
                    at.clone(),
                    "a wildcard grant cannot be narrowed per principal",
                );
            }
            if let Some(r) = &a.relation {
                self.subject_set(&at, &a.type_name, r);
            }
        }
        let kind = if ty == self.root {
            Kind::Direct
        } else {
            Kind::Via
        };
        self.out.insert(ViewRelation {
            resource_type: ty.to_string(),
            relation: name.to_string(),
            kind,
            target_type: None,
            through: through.map(str::to_string),
        });
    }

    /// `T#r` granted on a view relation: `r` must be a plain relation of object types on `T` (one level of groups).
    fn subject_set(&mut self, at: &str, ty: &str, rel: &str) {
        let Some(types) = self
            .schema
            .definitions
            .get(ty)
            .and_then(|d| d.relations.get(rel))
            .cloned()
        else {
            self.refuse(
                at.to_string(),
                "a subject set must name a relation (not a permission) of a defined type",
            );
            return;
        };
        for a in &types {
            if a.relation.is_some() || a.wildcard || a.caveat.is_some() || a.expiration {
                self.refuse(
                    format!("{ty}#{rel}"),
                    "a subject set's members must be plain object types (no nested sets, wildcards or caveats)",
                );
            }
        }
        self.out.insert(ViewRelation {
            resource_type: ty.to_string(),
            relation: rel.to_string(),
            kind: Kind::Set,
            target_type: None,
            through: None,
        });
    }

    fn expr(&mut self, ty: &str, name: &str, e: &Expr, through: Option<&str>) {
        let at = format!("{ty}#{name}");
        match e {
            Expr::Nil => {}
            Expr::Ref(r) => self.name(ty, r, through),
            Expr::Union(xs) => {
                for x in xs {
                    self.expr(ty, name, x, through);
                }
            }
            Expr::Intersect(_) => {
                self.refuse(at, "an intersection (&) is not supported in slice 1")
            }
            Expr::Exclude(_, _) => self.refuse(
                at,
                "an exclusion (-) is not monotone: removing a tuple can grant",
            ),
            Expr::Arrow {
                kind: ArrowKind::All,
                ..
            } => self.refuse(
                at,
                "an intersection arrow (.all) is not supported in slice 1",
            ),
            Expr::Arrow {
                tupleset,
                computed,
                kind: ArrowKind::Any,
            } => self.arrow(ty, name, tupleset, computed, through),
        }
    }

    fn arrow(
        &mut self,
        ty: &str,
        name: &str,
        tupleset: &str,
        computed: &str,
        through: Option<&str>,
    ) {
        let at = format!("{ty}#{name}");
        if ty != self.root || through.is_some() {
            self.refuse(
                at,
                "an arrow more than one hop from the projected type is not supported in slice 1",
            );
            return;
        }
        let Some(types) = self
            .schema
            .definitions
            .get(ty)
            .and_then(|d| d.relations.get(tupleset))
            .cloned()
        else {
            self.refuse(at, "an arrow's left side must be a relation");
            return;
        };
        for a in &types {
            let target = &a.type_name;
            if a.relation.is_some() || a.wildcard || a.caveat.is_some() || a.expiration {
                self.refuse(
                    format!("{ty}#{tupleset}"),
                    "an arrow's relation must hold plain object types",
                );
                continue;
            }
            self.out.insert(ViewRelation {
                resource_type: ty.to_string(),
                relation: tupleset.to_string(),
                kind: Kind::Arrow,
                target_type: Some(target.clone()),
                through: None,
            });
            if target == self.root {
                if computed != self.permission {
                    self.refuse(
                        format!("{ty}#{tupleset}"),
                        "a self-arrow must walk to the projected permission itself",
                    );
                }
                // A self-arrow to the projected permission adds nothing beyond itself: the walk is already there.
            } else {
                self.name(target, computed, Some(tupleset));
            }
        }
    }
}

#[cfg(test)]
mod tests;
