use crate::interface::{Compiler, Result};
use crate::passes::{self, BoxedResolver, BoxedGlobalCtxt};

use rustc_incremental::DepGraphFuture;
use rustc_data_structures::sync::Lrc;
use rustc_codegen_utils::codegen_backend::CodegenBackend;
use rustc::session::config::{OutputFilenames, OutputType};
use rustc::util::common::{time, ErrorReported};
use rustc::hir;
use rustc::lint;
use rustc::session::Session;
use rustc::lint::LintStore;
use rustc::hir::def_id::LOCAL_CRATE;
use rustc::ty::steal::Steal;
use rustc::ty::ResolverOutputs;
use rustc::dep_graph::DepGraph;
use std::cell::{Ref, RefMut, RefCell};
use std::rc::Rc;
use std::any::Any;
use std::mem;
use syntax::{self, ast};

/// Represent the result of a query.
/// This result can be stolen with the `take` method and returned with the `give` method.
pub struct Query<T> {
    result: RefCell<Option<Result<T>>>,
}

impl<T> Query<T> {
    fn compute<F: FnOnce() -> Result<T>>(&self, f: F) -> Result<&Query<T>> {
        let mut result = self.result.borrow_mut();
        if result.is_none() {
            *result = Some(f());
        }
        result.as_ref().unwrap().as_ref().map(|_| self).map_err(|err| *err)
    }

    /// Takes ownership of the query result. Further attempts to take or peek the query
    /// result will panic unless it is returned by calling the `give` method.
    pub fn take(&self) -> T {
        self.result
            .borrow_mut()
            .take()
            .expect("missing query result")
            .unwrap()
    }

    /// Borrows the query result using the RefCell. Panics if the result is stolen.
    pub fn peek(&self) -> Ref<'_, T> {
        Ref::map(self.result.borrow(), |r| {
            r.as_ref().unwrap().as_ref().expect("missing query result")
        })
    }

    /// Mutably borrows the query result using the RefCell. Panics if the result is stolen.
    pub fn peek_mut(&self) -> RefMut<'_, T> {
        RefMut::map(self.result.borrow_mut(), |r| {
            r.as_mut().unwrap().as_mut().expect("missing query result")
        })
    }
}

impl<T> Default for Query<T> {
    fn default() -> Self {
        Query {
            result: RefCell::new(None),
        }
    }
}

pub struct Queries<'comp> {
    compiler: &'comp Compiler,

    dep_graph_future: Query<Option<DepGraphFuture>>,
    parse: Query<ast::Crate>,
    crate_name: Query<String>,
    register_plugins: Query<(ast::Crate, Lrc<LintStore>)>,
    expansion: Query<(ast::Crate, Steal<Rc<RefCell<BoxedResolver>>>, Lrc<LintStore>)>,
    dep_graph: Query<DepGraph>,
    lower_to_hir: Query<(Steal<hir::map::Forest>, Steal<ResolverOutputs>)>,
    prepare_outputs: Query<OutputFilenames>,
    global_ctxt: Query<BoxedGlobalCtxt>,
    ongoing_codegen: Query<Box<dyn Any>>,
}

impl<'comp> Queries<'comp> {
    pub fn new(compiler: &'comp Compiler) -> Queries<'comp> {
        Queries {
            compiler,
            dep_graph_future: Default::default(),
            parse: Default::default(),
            crate_name: Default::default(),
            register_plugins: Default::default(),
            expansion: Default::default(),
            dep_graph: Default::default(),
            lower_to_hir: Default::default(),
            prepare_outputs: Default::default(),
            global_ctxt: Default::default(),
            ongoing_codegen: Default::default(),
        }
    }

    fn session(&self) -> &Lrc<Session> {
        &self.compiler.sess
    }
    fn codegen_backend(&self) -> &Lrc<Box<dyn CodegenBackend>> {
        &self.compiler.codegen_backend()
    }

    pub fn dep_graph_future(&self) -> Result<&Query<Option<DepGraphFuture>>> {
        self.dep_graph_future.compute(|| {
            Ok(if self.session().opts.build_dep_graph() {
                Some(rustc_incremental::load_dep_graph(self.session()))
            } else {
                None
            })
        })
    }

    pub fn parse(&self) -> Result<&Query<ast::Crate>> {
        self.parse.compute(|| {
            passes::parse(self.session(), &self.compiler.input).map_err(
                |mut parse_error| {
                    parse_error.emit();
                    ErrorReported
                },
            )
        })
    }

    pub fn register_plugins(&self) -> Result<&Query<(ast::Crate, Lrc<LintStore>)>> {
        self.register_plugins.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let krate = self.parse()?.take();

            let empty: &(dyn Fn(&Session, &mut lint::LintStore) + Sync + Send) = &|_, _| {};
            let result = passes::register_plugins(
                self.session(),
                &*self.codegen_backend().metadata_loader(),
                self.compiler.register_lints
                    .as_ref()
                    .map(|p| &**p)
                    .unwrap_or_else(|| empty),
                krate,
                &crate_name,
            );

            // Compute the dependency graph (in the background). We want to do
            // this as early as possible, to give the DepGraph maximum time to
            // load before dep_graph() is called, but it also can't happen
            // until after rustc_incremental::prepare_session_directory() is
            // called, which happens within passes::register_plugins().
            self.dep_graph_future().ok();

            result
        })
    }

    pub fn crate_name(&self) -> Result<&Query<String>> {
        self.crate_name.compute(|| {
            Ok(match self.compiler.crate_name {
                Some(ref crate_name) => crate_name.clone(),
                None => {
                    let parse_result = self.parse()?;
                    let krate = parse_result.peek();
                    rustc_codegen_utils::link::find_crate_name(
                        Some(self.session()),
                        &krate.attrs,
                        &self.compiler.input
                    )
                }
            })
        })
    }

    pub fn expansion(
        &self
    ) -> Result<&Query<(ast::Crate, Steal<Rc<RefCell<BoxedResolver>>>, Lrc<LintStore>)>> {
        self.expansion.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let (krate, lint_store) = self.register_plugins()?.take();
            passes::configure_and_expand(
                self.session().clone(),
                lint_store.clone(),
                self.codegen_backend().metadata_loader(),
                krate,
                &crate_name,
            ).map(|(krate, resolver)| {
                (krate, Steal::new(Rc::new(RefCell::new(resolver))), lint_store)
            })
        })
    }

    pub fn dep_graph(&self) -> Result<&Query<DepGraph>> {
        self.dep_graph.compute(|| {
            Ok(match self.dep_graph_future()?.take() {
                None => DepGraph::new_disabled(),
                Some(future) => {
                    let (prev_graph, prev_work_products) =
                        time(self.session(), "blocked while dep-graph loading finishes", || {
                            future.open().unwrap_or_else(|e| rustc_incremental::LoadResult::Error {
                                message: format!("could not decode incremental cache: {:?}", e),
                            }).open(self.session())
                        });
                    DepGraph::new(prev_graph, prev_work_products)
                }
            })
        })
    }

    pub fn lower_to_hir(
        &self,
    ) -> Result<&Query<(Steal<hir::map::Forest>, Steal<ResolverOutputs>)>> {
        self.lower_to_hir.compute(|| {
            let expansion_result = self.expansion()?;
            let peeked = expansion_result.peek();
            let krate = &peeked.0;
            let resolver = peeked.1.steal();
            let lint_store = &peeked.2;
            let hir = Steal::new(resolver.borrow_mut().access(|resolver| {
                passes::lower_to_hir(
                    self.session(),
                    lint_store,
                    resolver,
                    &*self.dep_graph()?.peek(),
                    &krate
                )
            })?);
            Ok((hir, Steal::new(BoxedResolver::to_resolver_outputs(resolver))))
        })
    }

    pub fn prepare_outputs(&self) -> Result<&Query<OutputFilenames>> {
        self.prepare_outputs.compute(|| {
            let expansion_result = self.expansion()?;
            let (krate, boxed_resolver, _) = &*expansion_result.peek();
            let crate_name = self.crate_name()?;
            let crate_name = crate_name.peek();
            passes::prepare_outputs(
                self.session(), self.compiler, &krate, &boxed_resolver, &crate_name
            )
        })
    }

    pub fn global_ctxt(&self) -> Result<&Query<BoxedGlobalCtxt>> {
        self.global_ctxt.compute(|| {
            let crate_name = self.crate_name()?.peek().clone();
            let outputs = self.prepare_outputs()?.peek().clone();
            let lint_store = self.expansion()?.peek().2.clone();
            let hir = self.lower_to_hir()?;
            let hir = hir.peek();
            let (hir_forest, resolver_outputs) = &*hir;
            Ok(passes::create_global_ctxt(
                self.compiler,
                lint_store,
                hir_forest.steal(),
                resolver_outputs.steal(),
                outputs,
                &crate_name))
        })
    }

    pub fn ongoing_codegen(&self) -> Result<&Query<Box<dyn Any>>> {
        self.ongoing_codegen.compute(|| {
            let outputs = self.prepare_outputs()?;
            self.global_ctxt()?.peek_mut().enter(|tcx| {
                tcx.analysis(LOCAL_CRATE).ok();

                // Don't do code generation if there were any errors
                self.session().compile_status()?;

                Ok(passes::start_codegen(
                    &***self.codegen_backend(),
                    tcx,
                    &*outputs.peek()
                ))
            })
        })
    }

    pub fn linker(self) -> Result<Linker> {
        let dep_graph = self.dep_graph()?;
        let prepare_outputs = self.prepare_outputs()?;
        let ongoing_codegen = self.ongoing_codegen()?;

        let sess = self.session().clone();
        let codegen_backend = self.codegen_backend().clone();

        Ok(Linker {
            sess,
            dep_graph: dep_graph.take(),
            prepare_outputs: prepare_outputs.take(),
            ongoing_codegen: ongoing_codegen.take(),
            codegen_backend,
        })
    }
}

pub struct Linker {
    sess: Lrc<Session>,
    dep_graph: DepGraph,
    prepare_outputs: OutputFilenames,
    ongoing_codegen: Box<dyn Any>,
    codegen_backend: Lrc<Box<dyn CodegenBackend>>,
}

impl Linker {
    pub fn link(self) -> Result<()> {
        self.codegen_backend.join_codegen_and_link(
            self.ongoing_codegen,
            &self.sess,
            &self.dep_graph,
            &self.prepare_outputs,
        ).map_err(|_| ErrorReported)
    }
}

impl Compiler {
    pub fn enter<'c, F, T>(&'c self, f: F) -> Result<T>
        where F: FnOnce(Queries<'c>) -> Result<T>
    {
        let queries = Queries::new(&self);
        f(queries)
    }

    // This method is different to all the other methods in `Compiler` because
    // it lacks a `Queries` entry. It's also not currently used. It does serve
    // as an example of how `Compiler` can be used, with additional steps added
    // between some passes. And see `rustc_driver::run_compiler` for a more
    // complex example.
    pub fn compile(&self) -> Result<()> {
        let linker = self.enter(|queries| {
            queries.prepare_outputs()?;

            if self.session().opts.output_types.contains_key(&OutputType::DepInfo)
                && self.session().opts.output_types.len() == 1
            {
                return Ok(None)
            }

            queries.global_ctxt()?;

            // Drop AST after creating GlobalCtxt to free memory.
            mem::drop(queries.expansion()?.take());

            queries.ongoing_codegen()?;

            // Drop GlobalCtxt after starting codegen to free memory.
            mem::drop(queries.global_ctxt()?.take());

            let linker = queries.linker()?;
            Ok(Some(linker))
        })?;

        if let Some(linker) = linker {
            linker.link()?
        }

        Ok(())
    }
}
