use std::{ops::ControlFlow, path::PathBuf};

use rustc_hash::FxHashSet;

use oxc::{
    CompilerInterface,
    allocator::Allocator,
    ast::{AstKind, Comment, ast::Program},
    codegen::{CodegenOptions, CodegenReturn},
    diagnostics::OxcDiagnostic,
    minifier::CompressOptions,
    parser::{ParseOptions, ParserReturn},
    regular_expression::{LiteralParser, Options},
    semantic::{Semantic, SemanticBuilderReturn},
    span::{ContentEq, SourceType, Span},
    transformer::{TransformOptions, TransformerReturn},
};
use oxc_tasks_transform_checker::{check_semantic_after_transform, check_semantic_ids};

use crate::suite::TestResult;

#[expect(clippy::struct_excessive_bools)]
#[derive(Default)]
pub struct Driver {
    pub path: PathBuf,
    // options
    pub transform: Option<TransformOptions>,
    pub compress: bool,
    pub remove_whitespace: bool,
    pub codegen: bool,
    pub check_semantic: bool,
    pub allow_return_outside_function: bool,
    // results
    pub panicked: bool,
    pub errors: Vec<OxcDiagnostic>,
    pub printed: String,
}

impl CompilerInterface for Driver {
    fn parse_options(&self) -> ParseOptions {
        ParseOptions {
            parse_regular_expression: true,
            allow_return_outside_function: self.allow_return_outside_function,
            ..ParseOptions::default()
        }
    }

    fn semantic_child_scope_ids(&self) -> bool {
        true
    }

    fn transform_options(&self) -> Option<&TransformOptions> {
        self.transform.as_ref()
    }

    fn compress_options(&self) -> Option<CompressOptions> {
        self.compress.then(CompressOptions::smallest)
    }

    fn codegen_options(&self) -> Option<CodegenOptions> {
        self.codegen
            .then(|| CodegenOptions { minify: self.remove_whitespace, ..CodegenOptions::default() })
    }

    fn handle_errors(&mut self, errors: Vec<OxcDiagnostic>) {
        self.errors.extend(errors);
    }

    fn after_parse(&mut self, parser_return: &mut ParserReturn) -> ControlFlow<()> {
        let ParserReturn { program, panicked, errors, .. } = parser_return;
        self.panicked = *panicked;
        if self.check_comments(&program.comments) {
            return ControlFlow::Break(());
        }
        if (errors.is_empty() || !*panicked) && program.source_type.is_unambiguous() {
            self.errors.push(OxcDiagnostic::error("SourceType must not be unambiguous."));
        }
        // Make sure serialization doesn't crash; also for code coverage.
        program.to_estree_ts_json();
        ControlFlow::Continue(())
    }

    fn after_semantic(&mut self, ret: &mut SemanticBuilderReturn) -> ControlFlow<()> {
        if self.check_semantic {
            let Some(root_node) = ret.semantic.nodes().root_node() else {
                return ControlFlow::Break(());
            };
            let AstKind::Program(program) = root_node.kind() else {
                return ControlFlow::Break(());
            };
            if let Some(errors) = check_semantic_ids(program) {
                self.errors.extend(errors);
                return ControlFlow::Break(());
            }
        };
        self.check_regular_expressions(&ret.semantic);
        ControlFlow::Continue(())
    }

    fn after_transform(
        &mut self,
        program: &mut Program<'_>,
        transformer_return: &mut TransformerReturn,
    ) -> ControlFlow<()> {
        if self.check_semantic {
            if let Some(errors) =
                check_semantic_after_transform(&transformer_return.scoping, program)
            {
                self.errors.extend(errors);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn after_codegen(&mut self, ret: CodegenReturn) {
        self.printed = ret.code;
    }
}

impl Driver {
    pub fn errors(&mut self) -> Vec<OxcDiagnostic> {
        std::mem::take(&mut self.errors)
    }

    pub fn idempotency(
        mut self,
        case: &'static str,
        source_text: &str,
        source_type: SourceType,
    ) -> TestResult {
        self.run(source_text, source_type);
        let printed1 = self.printed.clone();
        self.run(&printed1, source_type);
        let printed2 = self.printed.clone();
        if printed1 == printed2 {
            TestResult::Passed
        } else {
            TestResult::Mismatch(case, printed1, printed2)
        }
    }

    pub fn run(&mut self, source_text: &str, source_type: SourceType) {
        let path = self.path.clone();
        self.compile(source_text, source_type, &path);
    }

    fn check_comments(&mut self, comments: &[Comment]) -> bool {
        let mut uniq: FxHashSet<Span> = FxHashSet::default();
        for comment in comments {
            if !uniq.insert(comment.span) {
                self.errors
                    .push(OxcDiagnostic::error("Duplicate Comment").with_label(comment.span));
                return true;
            }
        }
        false
    }

    /// Idempotency test for printing regular expressions.
    fn check_regular_expressions(&mut self, semantic: &Semantic<'_>) {
        let allocator = Allocator::default();
        for literal in semantic.nodes().iter().filter_map(|node| node.kind().as_reg_exp_literal()) {
            let Some(pattern) = literal.regex.pattern.as_pattern() else {
                continue;
            };
            let printed1 = pattern.to_string();
            let flags = literal.regex.flags.to_inline_string();
            match LiteralParser::new(&allocator, &printed1, Some(&flags), Options::default())
                .parse()
            {
                Ok(pattern2) => {
                    let printed2 = pattern2.to_string();
                    if !pattern2.content_eq(pattern) {
                        self.errors.push(OxcDiagnostic::error(format!(
                        "Regular Expression content mismatch for `{}`: `{pattern}` == `{pattern2}`",
                        literal.span.source_text(semantic.source_text())
                        )));
                    }
                    if printed1 != printed2 {
                        self.errors.push(OxcDiagnostic::error(format!(
                            "Regular Expression mismatch: {printed1} {printed2}"
                        )));
                    }
                }
                Err(error) => {
                    self.errors.push(OxcDiagnostic::error(format!(
                        "Failed to re-parse `{}`, printed as `/{printed1}/{flags}`, {error}",
                        literal.span.source_text(semantic.source_text()),
                    )));
                }
            };
        }
    }
}
