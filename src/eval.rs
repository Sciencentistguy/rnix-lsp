use crate::error::{InternalError, ValueError};
use crate::parse::BinOpKind;
use crate::scope::*;
use crate::value::*;
use crate::EvalError;
use gc::{Finalize, Gc, GcCell, Trace};
use rnix::TextRange;
use std::borrow::Borrow;
use std::collections::HashMap;

// Expressions like BinOp have the only copy of their Expr children,
// so they use ExprResultBox. Expressions like Map, which may have
// contents copied in multiple places, need ExprResultGc.
type ExprResultBox = Result<Box<Expr>, EvalError>;
type ExprResultGc = Result<Gc<Expr>, EvalError>;

/// Used to lazily calculate the value of a Expr. This should be
/// tolerant of parsing and evaluation errors from child Exprs.
///
/// We store everything that we want the user to inspect. For example,
/// the source for an attribute key-value pair includes the key so the
/// user can hover inside dynamic keys in code like
/// `{ "${toString (1+1)}" = 2; }`.
#[derive(Debug, Trace, Finalize)]
pub enum ExprSource {
    // We want to share child MapAttrs between the ExprSource
    // and the value map, so we use Gc.
    AttrSet {
        /// We use a list because the user might define the same top-level
        /// attribute in multiple places via path syntax. For example:
        /// ```nix
        /// {
        ///   xyz.foo = true;
        ///   xyz.bar = false;
        /// }
        /// ```
        definitions: Vec<ExprResultGc>,
    },
    /// See the AttrSet handling in Expr::parse for more details.
    /// Note that this syntax is the exact opposite of Expr::Select.
    KeyValuePair {
        key: ExprResultGc,
        value: ExprResultGc,
    },
    /// Selection of an attribute from an AttrSet. This is used for
    /// multiple syntaxes, such as `inherit (xyz) foo` and `xyz.foo`.
    Select {
        /// We use Gc here because we need to share `from` across multiple
        /// Expr nodes for syntax like `inherit (xyz) foo bar`
        from: ExprResultGc,
        index: ExprResultBox,
    },
    /// Dynamic attribute, such as the curly braces in `foo.${toString (1+1)}`
    Dynamic {
        inner: ExprResultBox,
    },
    Ident {
        name: String,
    },
    Literal {
        value: NixValue,
    },
    Paren {
        inner: ExprResultBox,
    },
    BinOp {
        op: BinOpKind,
        left: ExprResultBox,
        right: ExprResultBox,
    },
    BoolAnd {
        left: ExprResultBox,
        right: ExprResultBox,
    },
    BoolOr {
        left: ExprResultBox,
        right: ExprResultBox,
    },
    Implication {
        left: ExprResultBox,
        right: ExprResultBox,
    },
    UnaryInvert {
        value: ExprResultBox,
    },
    UnaryNegate {
        value: ExprResultBox,
    },
}

/// Syntax node that has context and can be lazily evaluated.
#[derive(Trace, Finalize)]
pub struct Expr {
    #[unsafe_ignore_trace]
    pub range: Option<TextRange>,
    pub value: GcCell<Option<Gc<NixValue>>>,
    pub source: ExprSource,
    pub scope: Gc<Scope>,
}

impl std::fmt::Debug for Expr {
    // The scope can be recursive, so we don't want to print it by default
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expr")
            .field("value", &self.value)
            .field("source", &self.source)
            .field("range", &self.range)
            .finish()
    }
}

impl Expr {
    /// Lazily evaluate a Expr, caching its value
    pub fn eval(&self) -> Result<Gc<NixValue>, EvalError> {
        let mut value_borrow = match self.value.try_borrow_mut() {
            Ok(x) => x,
            Err(_) => {
                // We already borrow ourselves as mutable, so we called .eval() on ourself
                // from an .eval(), which is probably infinite recursion.
                return Err(EvalError::Internal(InternalError::Unimplemented(
                    "infinite recursion".to_string(),
                )));
            }
        };
        if let Some(ref value) = *value_borrow {
            Ok(value.clone())
        } else {
            // We can later build a stack trace by wrapping errors here
            let value = self.eval_uncached()?;
            *value_borrow = Some(value.clone());
            Ok(value)
        }
    }

    fn eval_uncached(&self) -> Result<Gc<NixValue>, EvalError> {
        match &self.source {
            ExprSource::Paren { inner } => inner.as_ref()?.eval(),
            ExprSource::Literal { value } => Ok(Gc::new(value.clone())),
            ExprSource::BoolAnd { left, right } => {
                if left.as_ref()?.eval()?.as_bool()? {
                    right.as_ref()?.eval()
                } else {
                    Ok(Gc::new(NixValue::Bool(false)))
                }
            }
            ExprSource::BoolOr { left, right } => {
                if !left.as_ref()?.eval()?.as_bool()? {
                    right.as_ref()?.eval()
                } else {
                    Ok(Gc::new(NixValue::Bool(true)))
                }
            }
            ExprSource::Implication { left, right } => {
                if !left.as_ref()?.eval()?.as_bool()? {
                    Ok(Gc::new(NixValue::Bool(true)))
                } else {
                    right.as_ref()?.eval()
                }
            }

            #[allow(clippy::enum_glob_use)]
            #[allow(clippy::float_cmp)]
            // We want to match the Nix reference implementation
            ExprSource::BinOp { op, left, right } => {
                use BinOpKind::*;
                use NixValue::*;

                // Workaround for "temporary value dropped while borrowed"
                // https://doc.rust-lang.org/error-index.html#E0716
                let left_tmp = left.as_ref()?.eval()?;
                let left_val = left_tmp.borrow();
                let right_tmp = right.as_ref()?.eval()?;
                let right_val = right_tmp.borrow();

                // Specially handle integer division by zero
                if let (Div, Integer(_), Integer(0)) = (op, left_val, right_val) {
                    return Err(EvalError::Value(ValueError::DivisionByZero));
                }

                macro_rules! match_binops {
                    ( arithmetic [ $( $arith_kind:pat => $arith_oper:tt, )+ ],
                      comparisons [ $( $comp_kind:pat => $comp_oper:tt, )+ ],
                      $( $pattern:pat => $expr:expr ),* ) => {
                        match (op, left_val, right_val) {
                            $(
                                ($arith_kind, Integer(x), Integer(y)) => Integer(x $arith_oper y),
                                ($arith_kind, Float(x), Float(y)) => Float(x $arith_oper y),
                                ($arith_kind, Integer(x), Float(y)) => Float((*x as f64) $arith_oper y),
                                ($arith_kind, Float(x), Integer(y)) => Float(x $arith_oper (*y as f64)),
                            )*
                            $(
                                ($comp_kind, Integer(x), Integer(y)) => Bool(x $comp_oper y),
                                ($comp_kind, Float(x), Float(y)) => Bool(x $comp_oper y),
                                ($comp_kind, Integer(x), Float(y)) => Bool((*x as f64) $comp_oper *y),
                                ($comp_kind, Float(x), Integer(y)) => Bool(*x $comp_oper (*y as f64)),
                            )*
                            $(
                                $pattern => $expr,
                            )*
                        }
                    };
                }

                let out = match_binops! {
                    arithmetic [
                        Add => +, Sub => -, Mul => *, Div => /,
                    ],
                    comparisons [
                        Equal => ==, NotEqual => !=,
                        Greater => >, GreaterOrEq => >=,
                        Less => <, LessOrEq => <=,
                    ],
                    _ => {
                        // We assume that it's our fault if an operation is unsupported.
                        // Over time, we can rewrite common cases into type errors.
                        return Err(EvalError::Internal(InternalError::Unimplemented(format!(
                            "{:?} {:?} {:?} unsupported",
                            left, op, right
                        ))))
                    }
                };

                Ok(Gc::new(out))
            }
            ExprSource::UnaryInvert { value } => {
                Ok(Gc::new(NixValue::Bool(!value.as_ref()?.eval()?.as_bool()?)))
            }
            ExprSource::UnaryNegate { value } => {
                Ok(Gc::new(match value.as_ref()?.eval()?.borrow() {
                    NixValue::Integer(x) => NixValue::Integer(-x),
                    NixValue::Float(x) => NixValue::Float(-x),
                    _ => {
                        return Err(EvalError::Value(ValueError::TypeError(
                            "cannot negate a non-number".to_string(),
                        )))
                    }
                }))
            }
            ExprSource::AttrSet { .. } => Err(EvalError::Internal(InternalError::Unexpected(
                "eval_uncached ExprSource::Map should be unreachable, ".to_string()
                    + "since the Expr::value should be initialized at creation",
            ))),
            ExprSource::KeyValuePair { value, .. } => value.as_ref()?.eval(),
            ExprSource::Dynamic { inner } => inner.as_ref()?.eval(),
            ExprSource::Ident { name } => self
                .scope
                .get(name)
                // We don't have everything implemented yet, so silently fail,
                // assuming we're at fault
                .ok_or_else(|| {
                    EvalError::Internal(InternalError::Unimplemented(format!(
                        "not found in scope: {}",
                        name
                    )))
                })?
                .eval(),
            ExprSource::Select { from, index } => {
                let key = index.as_ref()?.as_ident()?;
                let tmp = from.as_ref()?.eval()?;
                let map = tmp.as_map()?;
                let val = match map.get(&key) {
                    Some(x) => x,
                    None => {
                        // We don't have everything implemented yet, so silently fail,
                        // assuming we're at fault
                        return Err(EvalError::Internal(InternalError::Unimplemented(format!(
                            "missing key: {}",
                            key
                        ))));
                    }
                };
                val.eval()
            }
        }
    }

    /// Used for recursing to find the Expr at a cursor position.
    /// Note that if children have overlapping `range`s, then the
    /// first matching child will be used for tooling.
    pub fn children(&self) -> Vec<&Expr> {
        match &self.source {
            ExprSource::Paren { inner } => vec![inner],
            ExprSource::Literal { value: _ } => vec![],
            ExprSource::BinOp { op: _, left, right } => vec![left, right],
            ExprSource::BoolAnd { left, right } => vec![left, right],
            ExprSource::BoolOr { left, right } => vec![left, right],
            ExprSource::Implication { left, right } => vec![left, right],
            ExprSource::UnaryInvert { value } => vec![value],
            ExprSource::UnaryNegate { value } => vec![value],
            ExprSource::AttrSet { definitions } => {
                let mut out = vec![];
                out.extend(definitions);
                // This looks similar to code at the end of the function, but
                // we have Gc instead of Box, so we can't just return a vec
                // like the rest of the `match` arms.
                return out
                    .into_iter()
                    .map(|x| x.as_ref())
                    .filter_map(Result::ok)
                    .map(|x| x.as_ref())
                    .collect();
            }
            ExprSource::KeyValuePair { key, value } => {
                let mut out = vec![];
                if let Ok(x) = value {
                    out.push(x.as_ref());
                }
                if let Ok(x) = key {
                    if let ExprSource::Dynamic { inner: Ok(val) } = &x.source {
                        out.push(val.as_ref());
                    }
                }
                return out;
            }
            ExprSource::Dynamic { inner } => vec![inner],
            ExprSource::Ident { .. } => vec![],
            ExprSource::Select { from, index } => {
                let mut out = vec![];
                // For { .. }.x, we want hovering `x` to show the value.
                // However, we still want syntax like { .. }."${toString (1+1)}"
                // to allow interaction with the dynamic expression.
                if let Ok(x) = index {
                    if let ExprSource::Dynamic { inner: Ok(val) } = &x.source {
                        out.push(val.as_ref());
                    }
                }
                if let Ok(x) = from {
                    out.push(x.as_ref());
                }
                return out;
            }
        }
        .into_iter()
        .map(|x| x.as_ref())
        .filter_map(Result::ok)
        .map(|x| x.as_ref())
        .collect()
    }

    pub fn get_definition(&self) -> Option<Gc<Expr>> {
        use ExprSource::*;
        match &self.source {
            Ident { name } => self.scope.get(name),
            Select { from, index } => {
                let idx = index.as_ref().ok()?.as_ident().ok()?;
                let out = from
                    .as_ref()
                    .ok()?
                    .eval()
                    .ok()?
                    .as_map()
                    .ok()?
                    .get(&idx)?
                    .clone();
                if let ExprSource::KeyValuePair { ref key, .. } = out.source {
                    key.clone().ok()
                } else {
                    Some(out)
                }
            }
            _ => None,
        }
    }

    /// Interpret the expression as an identifier. For example:
    /// ```text
    /// foo => "foo"
    /// "foo" => "foo"
    /// "${"foo"}" => "foo"
    /// ```
    pub fn as_ident(&self) -> Result<String, EvalError> {
        use ExprSource::*;
        match &self.source {
            Ident { name } => Ok(name.clone()),
            Dynamic { inner } => inner.as_ref()?.eval()?.as_str(),
            Literal { value } => value.as_str(),
            _ => Err(EvalError::Internal(InternalError::Unimplemented(
                "unsupported identifier expression".to_string(),
            ))),
        }
    }
}

/// Used for merging sets during parsing. For example:
/// { a.b = 1; a.c = 2; } => { a = { b = 1; c = 2; }; }
pub fn merge_set_literal(name: String, a: Gc<Expr>, b: Gc<Expr>) -> Result<Gc<Expr>, EvalError> {
    // evaluate literal attr sets only, otherwise error
    let eval_literal = |src: Gc<Expr>| {
        let src = if let ExprSource::KeyValuePair { value, .. } = &src.source {
            value.as_ref()?.clone()
        } else {
            src
        };
        if let ExprSource::AttrSet { .. } = &src.source {
            src.eval()?.as_map()
        } else {
            // We cannot merge a literal with a non-literal. This error is
            // caused by incorrect expressions such as:
            // ```
            // repl> let x = { y = 1; }; in { a = x; a.z = 2; }
            // error: attribute 'a.z' at (string):1:33 already defined at (string):1:26
            // ```
            // The above would be caught because `x` is an ExprSource::Ident (as
            // opposed to being an ExprSource::AttrSet literal).
            Err(EvalError::Value(ValueError::AttrAlreadyDefined(
                name.to_string(),
            )))
        }
    };

    let a = eval_literal(a)?;
    let b = eval_literal(b)?;
    let mut out = HashMap::new();
    for (key, val) in a.iter() {
        let tmp = match b.get(key) {
            Some(x) => merge_set_literal(format!("{}.{}", name, key), x.clone(), val.clone())?,
            None => val.clone(),
        };
        out.insert(key.clone(), tmp);
    }
    for (key, val) in b.iter() {
        if !a.contains_key(key) {
            out.insert(key.clone(), val.clone());
        }
    }

    Ok(Gc::new(Expr {
        range: None,
        value: GcCell::new(None),
        source: ExprSource::Literal {
            value: NixValue::Map(out),
        },
        scope: Gc::new(Scope::None),
    }))
}
