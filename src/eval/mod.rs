//! Evaluation of markup into modules.

#[macro_use]
mod array;
#[macro_use]
mod dict;
#[macro_use]
mod value;
#[macro_use]
mod styles;
mod capture;
mod class;
mod function;
mod node;
mod ops;
mod scope;

pub use array::*;
pub use capture::*;
pub use class::*;
pub use dict::*;
pub use function::*;
pub use node::*;
pub use scope::*;
pub use styles::*;
pub use value::*;

use std::cell::RefMut;
use std::collections::HashMap;
use std::io;
use std::mem;
use std::path::PathBuf;

use unicode_segmentation::UnicodeSegmentation;

use crate::diag::{At, Error, StrResult, Trace, Tracepoint, TypResult};
use crate::geom::{Angle, Fractional, Length, Relative};
use crate::image::ImageStore;
use crate::layout::RootNode;
use crate::library::{self, TextNode};
use crate::loading::Loader;
use crate::source::{SourceId, SourceStore};
use crate::syntax::ast::*;
use crate::syntax::{Span, Spanned};
use crate::util::{EcoString, RefMutExt};
use crate::Context;

/// An evaluated module, ready for importing or conversion to a root layout
/// tree.
#[derive(Debug, Default, Clone)]
pub struct Module {
    /// The top-level definitions that were bound in this module.
    pub scope: Scope,
    /// The module's layoutable contents.
    pub node: Node,
}

impl Module {
    /// Convert this module's node into a layout tree.
    pub fn into_root(self) -> RootNode {
        self.node.into_root()
    }
}

/// Evaluate an expression.
pub trait Eval {
    /// The output of evaluating the expression.
    type Output;

    /// Evaluate the expression to the output value.
    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output>;
}

/// The context for evaluation.
pub struct EvalContext<'a> {
    /// The loader from which resources (files and images) are loaded.
    pub loader: &'a dyn Loader,
    /// Stores loaded source files.
    pub sources: &'a mut SourceStore,
    /// Stores decoded images.
    pub images: &'a mut ImageStore,
    /// The stack of imported files that led to evaluation of the current file.
    pub route: Vec<SourceId>,
    /// Caches imported modules.
    pub modules: HashMap<SourceId, Module>,
    /// The active scopes.
    pub scopes: Scopes<'a>,
    /// The active styles.
    pub styles: StyleMap,
}

impl<'a> EvalContext<'a> {
    /// Create a new evaluation context.
    pub fn new(ctx: &'a mut Context, source: SourceId) -> Self {
        Self {
            loader: ctx.loader.as_ref(),
            sources: &mut ctx.sources,
            images: &mut ctx.images,
            route: vec![source],
            modules: HashMap::new(),
            scopes: Scopes::new(Some(&ctx.std)),
            styles: StyleMap::new(),
        }
    }

    /// Process an import of a module relative to the current location.
    pub fn import(&mut self, path: &str, span: Span) -> TypResult<SourceId> {
        // Load the source file.
        let full = self.make_path(path);
        let id = self.sources.load(&full).map_err(|err| {
            Error::boxed(span, match err.kind() {
                io::ErrorKind::NotFound => "file not found".into(),
                _ => format!("failed to load source file ({})", err),
            })
        })?;

        // Prevent cyclic importing.
        if self.route.contains(&id) {
            bail!(span, "cyclic import");
        }

        // Check whether the module was already loaded.
        if self.modules.get(&id).is_some() {
            return Ok(id);
        }

        // Parse the file.
        let source = self.sources.get(id);
        let ast = source.ast()?;

        // Prepare the new context.
        let new_scopes = Scopes::new(self.scopes.base);
        let prev_scopes = mem::replace(&mut self.scopes, new_scopes);
        let prev_styles = mem::take(&mut self.styles);
        self.route.push(id);

        // Evaluate the module.
        let node = ast.eval(self).trace(|| Tracepoint::Import, span)?;

        // Restore the old context.
        let new_scopes = mem::replace(&mut self.scopes, prev_scopes);
        self.styles = prev_styles;
        self.route.pop().unwrap();

        // Save the evaluated module.
        let module = Module { scope: new_scopes.top, node };
        self.modules.insert(id, module);

        Ok(id)
    }

    /// Complete a user-entered path (relative to the source file) to be
    /// relative to the compilation environment's root.
    pub fn make_path(&self, path: &str) -> PathBuf {
        if let Some(&id) = self.route.last() {
            if let Some(dir) = self.sources.get(id).path().parent() {
                return dir.join(path);
            }
        }

        path.into()
    }
}

impl Eval for Markup {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let prev = mem::take(&mut ctx.styles);
        let nodes = self.nodes();
        let upper = nodes.size_hint().1.unwrap_or_default();
        let mut seq = Vec::with_capacity(upper);
        for piece in nodes {
            seq.push(Styled::new(piece.eval(ctx)?, ctx.styles.clone()));
        }
        ctx.styles = prev;
        Ok(Node::Sequence(seq))
    }
}

impl Eval for MarkupNode {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        Ok(match self {
            Self::Space => Node::Space,
            Self::Linebreak => Node::Linebreak,
            Self::Parbreak => Node::Parbreak,
            Self::Strong => {
                ctx.styles.toggle(TextNode::STRONG);
                Node::new()
            }
            Self::Emph => {
                ctx.styles.toggle(TextNode::EMPH);
                Node::new()
            }
            Self::Text(text) => Node::Text(text.clone()),
            Self::Raw(raw) => raw.eval(ctx)?,
            Self::Math(math) => math.eval(ctx)?,
            Self::Heading(heading) => heading.eval(ctx)?,
            Self::List(list) => list.eval(ctx)?,
            Self::Enum(enum_) => enum_.eval(ctx)?,
            Self::Expr(expr) => expr.eval(ctx)?.show(),
        })
    }
}

impl Eval for RawNode {
    type Output = Node;

    fn eval(&self, _: &mut EvalContext) -> TypResult<Self::Output> {
        let text = Node::Text(self.text.clone()).monospaced();
        Ok(if self.block {
            Node::Block(text.into_block())
        } else {
            text
        })
    }
}

impl Eval for MathNode {
    type Output = Node;

    fn eval(&self, _: &mut EvalContext) -> TypResult<Self::Output> {
        let text = Node::Text(self.formula.trim().into()).monospaced();
        Ok(if self.display {
            Node::Block(text.into_block())
        } else {
            text
        })
    }
}

impl Eval for HeadingNode {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        Ok(Node::block(library::HeadingNode {
            child: self.body().eval(ctx)?.into_block(),
            level: self.level(),
        }))
    }
}

impl Eval for ListNode {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        Ok(Node::block(library::ListNode {
            child: self.body().eval(ctx)?.into_block(),
            labelling: library::Unordered,
        }))
    }
}

impl Eval for EnumNode {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        Ok(Node::block(library::ListNode {
            child: self.body().eval(ctx)?.into_block(),
            labelling: library::Ordered(self.number()),
        }))
    }
}

impl Eval for Expr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        match self {
            Self::Lit(v) => v.eval(ctx),
            Self::Ident(v) => v.eval(ctx),
            Self::Array(v) => v.eval(ctx).map(Value::Array),
            Self::Dict(v) => v.eval(ctx).map(Value::Dict),
            Self::Template(v) => v.eval(ctx).map(Value::Node),
            Self::Group(v) => v.eval(ctx),
            Self::Block(v) => v.eval(ctx),
            Self::Call(v) => v.eval(ctx),
            Self::Closure(v) => v.eval(ctx),
            Self::With(v) => v.eval(ctx),
            Self::Unary(v) => v.eval(ctx),
            Self::Binary(v) => v.eval(ctx),
            Self::Let(v) => v.eval(ctx),
            Self::Set(v) => v.eval(ctx),
            Self::If(v) => v.eval(ctx),
            Self::While(v) => v.eval(ctx),
            Self::For(v) => v.eval(ctx),
            Self::Import(v) => v.eval(ctx),
            Self::Include(v) => v.eval(ctx),
        }
    }
}

impl Eval for Lit {
    type Output = Value;

    fn eval(&self, _: &mut EvalContext) -> TypResult<Self::Output> {
        Ok(match self.kind() {
            LitKind::None => Value::None,
            LitKind::Auto => Value::Auto,
            LitKind::Bool(v) => Value::Bool(v),
            LitKind::Int(v) => Value::Int(v),
            LitKind::Float(v) => Value::Float(v),
            LitKind::Length(v, unit) => Value::Length(Length::with_unit(v, unit)),
            LitKind::Angle(v, unit) => Value::Angle(Angle::with_unit(v, unit)),
            LitKind::Percent(v) => Value::Relative(Relative::new(v / 100.0)),
            LitKind::Fractional(v) => Value::Fractional(Fractional::new(v)),
            LitKind::Str(ref v) => Value::Str(v.clone()),
        })
    }
}

impl Eval for Ident {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        match ctx.scopes.get(self) {
            Some(slot) => Ok(slot.borrow().clone()),
            None => bail!(self.span(), "unknown variable"),
        }
    }
}

impl Eval for ArrayExpr {
    type Output = Array;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        self.items().map(|expr| expr.eval(ctx)).collect()
    }
}

impl Eval for DictExpr {
    type Output = Dict;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        self.items()
            .map(|x| Ok((x.name().take(), x.expr().eval(ctx)?)))
            .collect()
    }
}

impl Eval for TemplateExpr {
    type Output = Node;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        self.body().eval(ctx)
    }
}

impl Eval for GroupExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        self.expr().eval(ctx)
    }
}

impl Eval for BlockExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        ctx.scopes.enter();

        let mut output = Value::None;
        for expr in self.exprs() {
            let value = expr.eval(ctx)?;
            output = ops::join(output, value).at(expr.span())?;
        }

        ctx.scopes.exit();

        Ok(output)
    }
}

impl Eval for UnaryExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let value = self.expr().eval(ctx)?;
        let result = match self.op() {
            UnOp::Pos => ops::pos(value),
            UnOp::Neg => ops::neg(value),
            UnOp::Not => ops::not(value),
        };
        result.at(self.span())
    }
}

impl Eval for BinaryExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        match self.op() {
            BinOp::Add => self.apply(ctx, ops::add),
            BinOp::Sub => self.apply(ctx, ops::sub),
            BinOp::Mul => self.apply(ctx, ops::mul),
            BinOp::Div => self.apply(ctx, ops::div),
            BinOp::And => self.apply(ctx, ops::and),
            BinOp::Or => self.apply(ctx, ops::or),
            BinOp::Eq => self.apply(ctx, ops::eq),
            BinOp::Neq => self.apply(ctx, ops::neq),
            BinOp::Lt => self.apply(ctx, ops::lt),
            BinOp::Leq => self.apply(ctx, ops::leq),
            BinOp::Gt => self.apply(ctx, ops::gt),
            BinOp::Geq => self.apply(ctx, ops::geq),
            BinOp::Assign => self.assign(ctx, |_, b| Ok(b)),
            BinOp::AddAssign => self.assign(ctx, ops::add),
            BinOp::SubAssign => self.assign(ctx, ops::sub),
            BinOp::MulAssign => self.assign(ctx, ops::mul),
            BinOp::DivAssign => self.assign(ctx, ops::div),
        }
    }
}

impl BinaryExpr {
    /// Apply a basic binary operation.
    fn apply<F>(&self, ctx: &mut EvalContext, op: F) -> TypResult<Value>
    where
        F: FnOnce(Value, Value) -> StrResult<Value>,
    {
        let lhs = self.lhs().eval(ctx)?;

        // Short-circuit boolean operations.
        if (self.op() == BinOp::And && lhs == Value::Bool(false))
            || (self.op() == BinOp::Or && lhs == Value::Bool(true))
        {
            return Ok(lhs);
        }

        let rhs = self.rhs().eval(ctx)?;
        op(lhs, rhs).at(self.span())
    }

    /// Apply an assignment operation.
    fn assign<F>(&self, ctx: &mut EvalContext, op: F) -> TypResult<Value>
    where
        F: FnOnce(Value, Value) -> StrResult<Value>,
    {
        let rhs = self.rhs().eval(ctx)?;
        let mut target = self.lhs().access(ctx)?;
        let lhs = mem::take(&mut *target);
        *target = op(lhs, rhs).at(self.span())?;
        Ok(Value::None)
    }
}

impl Eval for CallExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let callee = self.callee().eval(ctx)?;
        let mut args = self.args().eval(ctx)?;

        match callee {
            Value::Array(array) => {
                array.get(args.into_index()?).map(Value::clone).at(self.span())
            }

            Value::Dict(dict) => {
                dict.get(args.into_key()?).map(Value::clone).at(self.span())
            }

            Value::Func(func) => {
                let point = || Tracepoint::Call(func.name().map(ToString::to_string));
                let value = func.call(ctx, &mut args).trace(point, self.span())?;
                args.finish()?;
                Ok(value)
            }

            Value::Class(class) => {
                let node = class.construct(ctx, &mut args)?;
                args.finish()?;
                Ok(Value::Node(node))
            }

            v => bail!(
                self.callee().span(),
                "expected callable or collection, found {}",
                v.type_name(),
            ),
        }
    }
}

impl Eval for CallArgs {
    type Output = Args;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let mut items = Vec::new();

        for arg in self.items() {
            let span = arg.span();
            match arg {
                CallArg::Pos(expr) => {
                    items.push(Arg {
                        span,
                        name: None,
                        value: Spanned::new(expr.eval(ctx)?, expr.span()),
                    });
                }
                CallArg::Named(named) => {
                    items.push(Arg {
                        span,
                        name: Some(named.name().take()),
                        value: Spanned::new(named.expr().eval(ctx)?, named.expr().span()),
                    });
                }
                CallArg::Spread(expr) => match expr.eval(ctx)? {
                    Value::None => {}
                    Value::Array(array) => {
                        items.extend(array.into_iter().map(|value| Arg {
                            span,
                            name: None,
                            value: Spanned::new(value, span),
                        }));
                    }
                    Value::Dict(dict) => {
                        items.extend(dict.into_iter().map(|(key, value)| Arg {
                            span,
                            name: Some(key),
                            value: Spanned::new(value, span),
                        }));
                    }
                    v => {
                        if let Value::Dyn(dynamic) = &v {
                            if let Some(args) = dynamic.downcast::<Args>() {
                                items.extend(args.items.iter().cloned());
                                continue;
                            }
                        }

                        bail!(expr.span(), "cannot spread {}", v.type_name())
                    }
                },
            }
        }

        Ok(Args { span: self.span(), items })
    }
}

impl Eval for ClosureExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        // Collect captured variables.
        let captured = {
            let mut visitor = CapturesVisitor::new(&ctx.scopes);
            visitor.visit(self.as_red());
            visitor.finish()
        };

        let mut sink = None;
        let mut params = Vec::new();

        // Collect parameters and an optional sink parameter.
        for param in self.params() {
            match param {
                ClosureParam::Pos(name) => {
                    params.push((name.take(), None));
                }
                ClosureParam::Named(named) => {
                    params.push((named.name().take(), Some(named.expr().eval(ctx)?)));
                }
                ClosureParam::Sink(name) => {
                    if sink.is_some() {
                        bail!(name.span(), "only one argument sink is allowed");
                    }
                    sink = Some(name.take());
                }
            }
        }

        // Clone the body expression so that we don't have a lifetime
        // dependence on the AST.
        let name = self.name().map(Ident::take);
        let body = self.body();

        // Define the actual function.
        let func = Function::new(name, move |ctx, args| {
            // Don't leak the scopes from the call site. Instead, we use the
            // scope of captured variables we collected earlier.
            let prev_scopes = mem::take(&mut ctx.scopes);
            ctx.scopes.top = captured.clone();

            // Parse the arguments according to the parameter list.
            for (param, default) in &params {
                ctx.scopes.def_mut(param, match default {
                    None => args.expect::<Value>(param)?,
                    Some(default) => {
                        args.named::<Value>(param)?.unwrap_or_else(|| default.clone())
                    }
                });
            }

            // Put the remaining arguments into the sink.
            if let Some(sink) = &sink {
                dynamic! {
                    Args: "arguments",
                }

                ctx.scopes.def_mut(sink, args.take());
            }

            let value = body.eval(ctx)?;
            ctx.scopes = prev_scopes;
            Ok(value)
        });

        Ok(Value::Func(func))
    }
}

impl Eval for WithExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let callee = self.callee();
        let wrapped = callee.eval(ctx)?.cast::<Function>().at(callee.span())?;
        let applied = self.args().eval(ctx)?;

        let name = wrapped.name().cloned();
        let func = Function::new(name, move |ctx, args| {
            args.items.splice(.. 0, applied.items.iter().cloned());
            wrapped.call(ctx, args)
        });

        Ok(Value::Func(func))
    }
}

impl Eval for LetExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let value = match self.init() {
            Some(expr) => expr.eval(ctx)?,
            None => Value::None,
        };
        ctx.scopes.def_mut(self.binding().take(), value);
        Ok(Value::None)
    }
}

impl Eval for SetExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let class = self.class();
        let class = class.eval(ctx)?.cast::<Class>().at(class.span())?;
        let mut args = self.args().eval(ctx)?;
        class.set(&mut args, &mut ctx.styles)?;
        args.finish()?;
        Ok(Value::None)
    }
}

impl Eval for IfExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let condition = self.condition();
        if condition.eval(ctx)?.cast::<bool>().at(condition.span())? {
            self.if_body().eval(ctx)
        } else if let Some(else_body) = self.else_body() {
            else_body.eval(ctx)
        } else {
            Ok(Value::None)
        }
    }
}

impl Eval for WhileExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let mut output = Value::None;

        let condition = self.condition();
        while condition.eval(ctx)?.cast::<bool>().at(condition.span())? {
            let body = self.body();
            let value = body.eval(ctx)?;
            output = ops::join(output, value).at(body.span())?;
        }

        Ok(output)
    }
}

impl Eval for ForExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        macro_rules! iter {
            (for ($($binding:ident => $value:ident),*) in $iter:expr) => {{
                let mut output = Value::None;
                ctx.scopes.enter();

                #[allow(unused_parens)]
                for ($($value),*) in $iter {
                    $(ctx.scopes.def_mut(&$binding, $value);)*

                    let value = self.body().eval(ctx)?;
                    output = ops::join(output, value)
                        .at(self.body().span())?;
                }

                ctx.scopes.exit();
                return Ok(output);
            }};
        }

        let iter = self.iter().eval(ctx)?;
        let pattern = self.pattern();
        let key = pattern.key().map(Ident::take);
        let value = pattern.value().take();

        match (key, value, iter) {
            (None, v, Value::Str(string)) => {
                iter!(for (v => value) in string.graphemes(true));
            }
            (None, v, Value::Array(array)) => {
                iter!(for (v => value) in array.into_iter());
            }
            (Some(i), v, Value::Array(array)) => {
                iter!(for (i => idx, v => value) in array.into_iter().enumerate());
            }
            (None, v, Value::Dict(dict)) => {
                iter!(for (v => value) in dict.into_iter().map(|p| p.1));
            }
            (Some(k), v, Value::Dict(dict)) => {
                iter!(for (k => key, v => value) in dict.into_iter());
            }
            (_, _, Value::Str(_)) => {
                bail!(pattern.span(), "mismatched pattern");
            }
            (_, _, iter) => {
                bail!(self.iter().span(), "cannot loop over {}", iter.type_name());
            }
        }
    }
}

impl Eval for ImportExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let path = self.path();
        let resolved = path.eval(ctx)?.cast::<EcoString>().at(path.span())?;
        let file = ctx.import(&resolved, path.span())?;
        let module = &ctx.modules[&file];

        match self.imports() {
            Imports::Wildcard => {
                for (var, slot) in module.scope.iter() {
                    ctx.scopes.def_mut(var, slot.borrow().clone());
                }
            }
            Imports::Items(idents) => {
                for ident in idents {
                    if let Some(slot) = module.scope.get(&ident) {
                        ctx.scopes.def_mut(ident.take(), slot.borrow().clone());
                    } else {
                        bail!(ident.span(), "unresolved import");
                    }
                }
            }
        }

        Ok(Value::None)
    }
}

impl Eval for IncludeExpr {
    type Output = Value;

    fn eval(&self, ctx: &mut EvalContext) -> TypResult<Self::Output> {
        let path = self.path();
        let resolved = path.eval(ctx)?.cast::<EcoString>().at(path.span())?;
        let file = ctx.import(&resolved, path.span())?;
        let module = &ctx.modules[&file];
        Ok(Value::Node(module.node.clone()))
    }
}

/// Try to mutably access the value an expression points to.
///
/// This only works if the expression is a valid lvalue.
pub trait Access {
    /// Try to access the value.
    fn access<'a>(&self, ctx: &'a mut EvalContext) -> TypResult<RefMut<'a, Value>>;
}

impl Access for Expr {
    fn access<'a>(&self, ctx: &'a mut EvalContext) -> TypResult<RefMut<'a, Value>> {
        match self {
            Expr::Ident(ident) => ident.access(ctx),
            Expr::Call(call) => call.access(ctx),
            _ => bail!(self.span(), "cannot access this expression mutably"),
        }
    }
}

impl Access for Ident {
    fn access<'a>(&self, ctx: &'a mut EvalContext) -> TypResult<RefMut<'a, Value>> {
        match ctx.scopes.get(self) {
            Some(slot) => match slot.try_borrow_mut() {
                Ok(guard) => Ok(guard),
                Err(_) => bail!(self.span(), "cannot mutate a constant"),
            },
            None => bail!(self.span(), "unknown variable"),
        }
    }
}

impl Access for CallExpr {
    fn access<'a>(&self, ctx: &'a mut EvalContext) -> TypResult<RefMut<'a, Value>> {
        let args = self.args().eval(ctx)?;
        let guard = self.callee().access(ctx)?;

        RefMut::try_map(guard, |value| match value {
            Value::Array(array) => array.get_mut(args.into_index()?).at(self.span()),
            Value::Dict(dict) => Ok(dict.get_mut(args.into_key()?)),
            v => bail!(
                self.callee().span(),
                "expected collection, found {}",
                v.type_name(),
            ),
        })
    }
}
