use std::{error::Error, path::PathBuf};

use lazy_static::lazy_static;
use regex::Regex;
use roc_can::{
    expr::Declarations,
    traverse::{find_ability_member_and_owning_type_at, find_type_at},
};
use roc_load::LoadedModule;
use roc_module::symbol::{Interns, ModuleId};
use roc_packaging::cache::RocCacheDir;
use roc_problem::can::Problem;
use roc_region::all::{LineColumn, LineColumnRegion, LineInfo, Region};
use roc_reporting::report::{can_problem, type_problem, RocDocAllocator};
use roc_solve_problem::TypeError;
use roc_types::pretty_print::{name_and_print_var, DebugPrint};

fn promote_expr_to_module(src: &str) -> String {
    let mut buffer = String::from(indoc::indoc!(
        r#"
        app "test"
            imports []
            provides [main] to "./platform"

        main =
        "#
    ));

    for line in src.lines() {
        // indent the body!
        buffer.push_str("    ");
        buffer.push_str(line);
        buffer.push('\n');
    }

    buffer
}

pub fn run_load_and_infer(src: &str) -> Result<(LoadedModule, String), std::io::Error> {
    use bumpalo::Bump;
    use tempfile::tempdir;

    let arena = &Bump::new();

    let module_src;
    let temp;
    if src.starts_with("app") {
        // this is already a module
        module_src = src;
    } else {
        // this is an expression, promote it to a module
        temp = promote_expr_to_module(src);
        module_src = &temp;
    }

    let loaded = {
        let dir = tempdir()?;
        let filename = PathBuf::from("Test.roc");
        let file_path = dir.path().join(filename);
        let result = roc_load::load_and_typecheck_str(
            arena,
            file_path,
            module_src,
            dir.path().to_path_buf(),
            roc_target::TargetInfo::default_x86_64(),
            roc_reporting::report::RenderTarget::Generic,
            RocCacheDir::Disallowed,
            roc_reporting::report::DEFAULT_PALETTE,
        );

        dir.close()?;

        result
    };

    let loaded = loaded.expect("failed to load module");
    Ok((loaded, module_src.to_string()))
}

pub fn format_problems(
    src: &str,
    home: ModuleId,
    interns: &Interns,
    can_problems: Vec<Problem>,
    type_problems: Vec<TypeError>,
) -> (String, String) {
    let filename = PathBuf::from("test.roc");
    let src_lines: Vec<&str> = src.split('\n').collect();
    let lines = LineInfo::new(src);
    let alloc = RocDocAllocator::new(&src_lines, home, interns);

    let mut can_reports = vec![];
    let mut type_reports = vec![];

    for problem in can_problems {
        let report = can_problem(&alloc, &lines, filename.clone(), problem.clone());
        can_reports.push(report.pretty(&alloc));
    }

    for problem in type_problems {
        if let Some(report) = type_problem(&alloc, &lines, filename.clone(), problem.clone()) {
            type_reports.push(report.pretty(&alloc));
        }
    }

    let mut can_reports_buf = String::new();
    let mut type_reports_buf = String::new();
    use roc_reporting::report::CiWrite;
    alloc
        .stack(can_reports)
        .1
        .render_raw(70, &mut CiWrite::new(&mut can_reports_buf))
        .unwrap();
    alloc
        .stack(type_reports)
        .1
        .render_raw(70, &mut CiWrite::new(&mut type_reports_buf))
        .unwrap();

    (can_reports_buf, type_reports_buf)
}

lazy_static! {
    /// Queries of the form
    ///
    /// ```
    /// ^^^{(directive),*}?
    ///
    /// directive :=
    ///   -\d+   # shift the query left by N columns
    ///   inst   # instantiate the given generic instance
    /// ```
    static ref RE_TYPE_QUERY: Regex =
        Regex::new(r#"(?P<where>\^+)(?:\{-(?P<sub>\d+)\})?"#).unwrap();
}

#[derive(Debug, Clone)]
pub struct TypeQuery {
    query_region: Region,
    source: String,
    comment_column: u32,
    source_line_column: LineColumn,
}

/// Parse inference queries in a Roc program.
/// See [RE_TYPE_QUERY].
fn parse_queries(src: &str) -> Vec<TypeQuery> {
    let line_info = LineInfo::new(src);
    let mut queries = vec![];
    let mut consecutive_query_lines = 0;
    for (i, line) in src.lines().enumerate() {
        // If this is a query line, it should start with a comment somewhere before the query
        // lines.
        let comment_column = match line.find("#") {
            Some(i) => i as _,
            None => {
                consecutive_query_lines = 0;
                continue;
            }
        };

        let mut queries_on_line = RE_TYPE_QUERY.captures_iter(line).into_iter().peekable();

        if queries_on_line.peek().is_none() {
            consecutive_query_lines = 0;
            continue;
        } else {
            consecutive_query_lines += 1;
        }

        for capture in queries_on_line {
            let source = capture
                .get(0)
                .expect("full capture must always exist")
                .as_str()
                .to_string();

            let wher = capture.name("where").unwrap();
            let subtract_col = capture
                .name("sub")
                .and_then(|m| str::parse(m.as_str()).ok())
                .unwrap_or(0);

            let (source_start, source_end) = (wher.start() as u32, wher.end() as u32);
            let (query_start, query_end) = (source_start - subtract_col, source_end - subtract_col);

            let source_line_column = LineColumn {
                line: i as u32,
                column: source_start,
            };

            let query_region = {
                let last_line = i as u32 - consecutive_query_lines;
                let query_start_lc = LineColumn {
                    line: last_line,
                    column: query_start,
                };
                let query_end_lc = LineColumn {
                    line: last_line,
                    column: query_end,
                };
                let query_lc_region = LineColumnRegion::new(query_start_lc, query_end_lc);
                line_info.convert_line_column_region(query_lc_region)
            };

            queries.push(TypeQuery {
                query_region,
                source,
                comment_column,
                source_line_column,
            });
        }
    }
    queries
}

#[derive(Default, Clone, Copy)]
pub struct InferOptions {
    pub print_can_decls: bool,
    pub print_only_under_alias: bool,
    pub allow_errors: bool,
}

pub struct InferredQuery {
    pub output: String,
    /// Where the comment before the query string was written in the source.
    pub comment_column: u32,
    /// Where the query string "^^^" itself was written in the source.
    pub source_line_column: LineColumn,
    /// The content of the query string.
    pub source: String,
}

pub struct InferredProgram {
    home: ModuleId,
    interns: Interns,
    declarations: Declarations,
    inferred_queries: Vec<InferredQuery>,
}

impl InferredProgram {
    /// Returns all inferred queries, sorted by their source location.
    pub fn into_sorted_queries(self) -> Vec<InferredQuery> {
        let mut inferred = self.inferred_queries;
        inferred.sort_by_key(|iq| iq.source_line_column);
        inferred
    }
}

pub fn infer_queries(src: &str, options: InferOptions) -> Result<InferredProgram, Box<dyn Error>> {
    let (
        LoadedModule {
            module_id: home,
            mut can_problems,
            mut type_problems,
            mut declarations_by_id,
            mut solved,
            interns,
            abilities_store,
            ..
        },
        src,
    ) = run_load_and_infer(src)?;

    let declarations = declarations_by_id.remove(&home).unwrap();
    let subs = solved.inner_mut();

    let can_problems = can_problems.remove(&home).unwrap_or_default();
    let type_problems = type_problems.remove(&home).unwrap_or_default();

    if !options.allow_errors {
        let (can_problems, type_problems) =
            format_problems(&src, home, &interns, can_problems, type_problems);

        if !can_problems.is_empty() {
            return Err(format!("Canonicalization problems: {can_problems}",).into());
        }
        if !type_problems.is_empty() {
            return Err(format!("Type problems: {type_problems}",).into());
        }
    }

    let queries = parse_queries(&src);
    if queries.is_empty() {
        return Err("No queries provided!".into());
    }

    let mut inferred_queries = Vec::with_capacity(queries.len());
    for TypeQuery {
        query_region,
        source,
        comment_column,
        source_line_column,
    } in queries.into_iter()
    {
        let start = query_region.start().offset;
        let end = query_region.end().offset;
        let text = &src[start as usize..end as usize];
        let var = find_type_at(query_region, &declarations)
            .ok_or_else(|| format!("No type for {:?} ({:?})!", &text, query_region))?;

        let snapshot = subs.snapshot();
        let actual_str = name_and_print_var(
            var,
            subs,
            home,
            &interns,
            DebugPrint {
                print_lambda_sets: true,
                print_only_under_alias: options.print_only_under_alias,
                ignore_polarity: true,
                print_weakened_vars: true,
            },
        );
        subs.rollback_to(snapshot);

        let elaborated = match find_ability_member_and_owning_type_at(
            query_region,
            &declarations,
            &abilities_store,
        ) {
            Some((spec_type, spec_symbol)) => {
                format!(
                    "{}#{}({}) : {}",
                    spec_type.as_str(&interns),
                    text,
                    spec_symbol.ident_id().index(),
                    actual_str
                )
            }
            None => {
                format!("{} : {}", text, actual_str)
            }
        };

        inferred_queries.push(InferredQuery {
            output: elaborated,
            comment_column,
            source_line_column,
            source,
        });
    }

    Ok(InferredProgram {
        home,
        interns,
        declarations,
        inferred_queries,
    })
}

pub fn infer_queries_help(src: &str, expected: impl FnOnce(&str), options: InferOptions) {
    let InferredProgram {
        home,
        interns,
        declarations: decls,
        inferred_queries,
    } = infer_queries(src, options).unwrap();

    let mut output_parts = Vec::with_capacity(inferred_queries.len() + 2);

    if options.print_can_decls {
        use roc_can::debug::{pretty_print_declarations, PPCtx};
        let ctx = PPCtx {
            home,
            interns: &interns,
            print_lambda_names: true,
        };
        let pretty_decls = pretty_print_declarations(&ctx, &decls);
        output_parts.push(pretty_decls);
        output_parts.push("\n".to_owned());
    }

    for InferredQuery { output, .. } in inferred_queries {
        output_parts.push(output);
    }

    let pretty_output = output_parts.join("\n");

    expected(&pretty_output);
}
