//! Layouting of syntax trees.

use crate::style::LayoutStyle;
use crate::syntax::decoration::Decoration;
use crate::syntax::span::{Span, Spanned};
use crate::syntax::tree::{CallExpr, SyntaxNode, SyntaxTree, CodeBlockExpr};
use crate::{DynFuture, Feedback, Pass};
use super::line::{LineContext, LineLayouter};
use super::text::{layout_text, TextContext};
use super::*;

/// Layout a syntax tree into a collection of boxes.
pub async fn layout_tree(
    tree: &SyntaxTree,
    ctx: LayoutContext<'_>,
) -> Pass<MultiLayout> {
    let mut layouter = TreeLayouter::new(ctx);
    layouter.layout_tree(tree).await;
    layouter.finish()
}

/// Performs the tree layouting.
struct TreeLayouter<'a> {
    ctx: LayoutContext<'a>,
    layouter: LineLayouter,
    style: LayoutStyle,
    feedback: Feedback,
}

impl<'a> TreeLayouter<'a> {
    fn new(ctx: LayoutContext<'a>) -> Self {
        Self {
            layouter: LineLayouter::new(LineContext {
                spaces: ctx.spaces.clone(),
                axes: ctx.axes,
                align: ctx.align,
                repeat: ctx.repeat,
                line_spacing: ctx.style.text.line_spacing(),
            }),
            style: ctx.style.clone(),
            ctx,
            feedback: Feedback::new(),
        }
    }

    fn finish(self) -> Pass<MultiLayout> {
        Pass::new(self.layouter.finish(), self.feedback)
    }

    fn layout_tree<'t>(&'t mut self, tree: &'t SyntaxTree) -> DynFuture<'t, ()> {
        Box::pin(async move {
            for node in tree {
                self.layout_node(node).await;
            }
        })
    }

    async fn layout_node(&mut self, node: &Spanned<SyntaxNode>) {
        let decorate = |this: &mut Self, deco| {
            this.feedback.decorations.push(Spanned::new(deco, node.span));
        };

        match &node.v {
            SyntaxNode::Spacing => self.layout_space(),
            SyntaxNode::Linebreak => self.layouter.finish_line(),

            SyntaxNode::ToggleItalic => {
                self.style.text.italic = !self.style.text.italic;
                decorate(self, Decoration::Italic);
            }
            SyntaxNode::ToggleBolder => {
                self.style.text.bolder = !self.style.text.bolder;
                decorate(self, Decoration::Bold);
            }

            SyntaxNode::Text(text) => {
                if self.style.text.italic { decorate(self, Decoration::Italic); }
                if self.style.text.bolder { decorate(self, Decoration::Bold); }
                self.layout_text(text).await;
            }

            SyntaxNode::Raw(lines) => self.layout_raw(lines).await,
            SyntaxNode::CodeBlock(block) => self.layout_code(block).await,
            SyntaxNode::Par(par) => self.layout_par(par).await,
            SyntaxNode::Call(call) => {
                self.layout_call(Spanned::new(call, node.span)).await;
            }
        }
    }

    fn layout_space(&mut self) {
        self.layouter.add_primary_spacing(
            self.style.text.word_spacing(),
            SpacingKind::WORD,
        );
    }

    async fn layout_text(&mut self, text: &str) {
        self.layouter.add(
            layout_text(
                text,
                TextContext {
                    loader: &self.ctx.loader,
                    style: &self.style.text,
                    dir: self.ctx.axes.primary,
                    align: self.ctx.align,
                }
            ).await
        );
    }

    async fn layout_raw(&mut self, lines: &[String]) {
        // TODO: Make this more efficient.
        let fallback = self.style.text.fallback.clone();
        self.style.text.fallback
            .list_mut()
            .insert(0, "monospace".to_string());
        self.style.text.fallback.flatten();

        let mut first = true;
        for line in lines {
            if !first {
                self.layouter.finish_line();
            }
            first = false;
            self.layout_text(line).await;
        }

        self.style.text.fallback = fallback;
    }

    async fn layout_code(&mut self, block: &CodeBlockExpr) {
        let fallback = self.style.text.fallback.clone();
        self.style.text.fallback
            .list_mut()
            .insert(0, "monospace".to_string());
        self.style.text.fallback.flatten();

        for line in &block.raw {
            self.layout_text(line).await;
            self.layouter.finish_line();
        }

        self.style.text.fallback = fallback;
    }

    async fn layout_par(&mut self, par: &SyntaxTree) {
        self.layout_tree(par).await;
        self.layouter.add_secondary_spacing(
            self.style.text.paragraph_spacing(),
            SpacingKind::PARAGRAPH,
        );
    }

    async fn layout_call(&mut self, call: Spanned<&CallExpr>) {
        let ctx = LayoutContext {
            style: &self.style,
            spaces: self.layouter.remaining(),
            root: false,
            ..self.ctx
        };

        let val = call.v.eval(&ctx, &mut self.feedback).await;
        let commands = Spanned::new(val, call.span).into_commands();

        for command in commands {
            self.execute_command(command, call.span).await;
        }
    }

    async fn execute_command(&mut self, command: Command, span: Span) {
        use Command::*;

        match command {
            LayoutSyntaxTree(tree) => self.layout_tree(&tree).await,

            Add(layout) => self.layouter.add(layout),
            AddMultiple(layouts) => self.layouter.add_multiple(layouts),
            AddSpacing(space, kind, axis) => match axis {
                Primary => self.layouter.add_primary_spacing(space, kind),
                Secondary => self.layouter.add_secondary_spacing(space, kind),
            }

            BreakLine => self.layouter.finish_line(),
            BreakPage => {
                if self.ctx.root {
                    self.layouter.finish_space(true)
                } else {
                    error!(
                        @self.feedback, span,
                        "page break cannot only be issued from root context",
                    );
                }
            }

            SetTextStyle(style) => {
                self.layouter.set_line_spacing(style.line_spacing());
                self.style.text = style;
            }
            SetPageStyle(style) => {
                if self.ctx.root {
                    self.style.page = style;

                    // The line layouter has no idea of page styles and thus we
                    // need to recompute the layouting space resulting of the
                    // new page style and update it within the layouter.
                    let margins = style.margins();
                    self.ctx.base = style.size.unpadded(margins);
                    self.layouter.set_spaces(vec![
                        LayoutSpace {
                            size: style.size,
                            padding: margins,
                            expansion: LayoutExpansion::new(true, true),
                        }
                    ], true);
                } else {
                    error!(
                        @self.feedback, span,
                        "page style cannot only be changed from root context",
                    );
                }
            }

            SetAlignment(align) => self.ctx.align = align,
            SetAxes(axes) => {
                self.layouter.set_axes(axes);
                self.ctx.axes = axes;
            }
        }
    }
}
