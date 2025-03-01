use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_hir as hir;
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::mir::mono::MonoItem;
use rustc_middle::ty::Instance;
use rustc_mir::monomorphize::collector::MonoItemCollectionMode;
use rustc_session::{declare_lint_pass, declare_tool_lint};
use rustc_span::source_map::Spanned;
use rustc_span::symbol::sym;

declare_tool_lint! {
    pub klint::INFALLIBLE_ALLOCATION,
    Warn,
    ""
}

declare_lint_pass!(InfallibleAllocation => [INFALLIBLE_ALLOCATION]);

impl<'tcx> LateLintPass<'tcx> for InfallibleAllocation {
    fn check_crate(&mut self, cx: &LateContext<'tcx>, _: &'tcx hir::Crate<'tcx>) {
        // Collect all mono items to be codegened with this crate. Discard the inline map, it does
        // not contain enough information for us; we will collect them ourselves later.
        //
        // Use eager mode here so dead code is also linted on.
        let access_map = super::monomorphize_collector::collect_crate_mono_items(
            cx.tcx,
            MonoItemCollectionMode::Eager,
        )
        .1;

        // Build a forward and backward dependency graph with span information.
        let mut forward = FxHashMap::default();
        let mut backward = FxHashMap::<_, Vec<_>>::default();

        access_map.iter_accesses(|accessor, accessees| {
            let accessor = match accessor {
                MonoItem::Static(s) => Instance::mono(cx.tcx, s),
                MonoItem::Fn(v) => v,
                _ => return,
            };

            let fwd_list = forward
                .entry(accessor)
                .or_insert_with(|| Vec::with_capacity(accessees.len()));
            let mut def_span = None;

            for accessee in accessees {
                let accessee_node = match accessee.node {
                    MonoItem::Static(s) => Instance::mono(cx.tcx, s),
                    MonoItem::Fn(v) => v,
                    _ => return,
                };

                // For const-evaluated items, they're collected from miri, which does not have span
                // information. Synthesize one with the accessor.
                let span = if accessee.span.is_dummy() {
                    *def_span.get_or_insert_with(|| cx.tcx.def_span(accessor.def_id()))
                } else {
                    accessee.span
                };

                fwd_list.push(Spanned {
                    node: accessee_node,
                    span,
                });
                backward.entry(accessee_node).or_default().push(Spanned {
                    node: accessor,
                    span,
                });
            }
        });

        // Find all fallible functions
        let mut visited = FxHashSet::default();

        for accessee in backward.keys() {
            let name = cx.tcx.def_path_str(accessee.def_id());

            // Anything (directly) called by assume_fallible is considered to be fallible.
            if name.contains("assume_fallible") {
                visited.insert(*accessee);
                for accessor in forward.get(&accessee).unwrap_or(&Vec::new()) {
                    visited.insert(accessor.node);
                }
                continue;
            }

            match name.as_str() {
                // These are fallible allocation functions that return null ptr on failure.
                "alloc::alloc::__rust_alloc"
                | "alloc::alloc::__rust_alloc_zeroed"
                | "alloc::alloc::__rust_realloc"
                | "alloc::alloc::__rust_dealloc"
                // Fallible allocation function
                | "alloc::string::String::try_reserve"
                | "alloc::string::String::try_reserve_exact" => {
                    visited.insert(*accessee);
                }
                _ => (),
            }
        }

        let mut infallible = FxHashSet::default();
        let mut work_queue = Vec::new();
        for accessee in backward.keys() {
            // Only go-through non-local-copy items.
            // This allows us to not to be concerned about `len()`, `is_empty()`,
            // because they are all inlineable.
            if forward.contains_key(accessee) {
                continue;
            }

            if cx.tcx.original_crate_name(accessee.def_id().krate) == sym::alloc {
                // If this item originates from alloc crate, mark it as infallible.
                // Add item to the allowlist above if there are false positives.
                work_queue.push(*accessee);
            }
        }

        // Propagate infallible property.
        while let Some(work_item) = work_queue.pop() {
            if visited.contains(&work_item) {
                continue;
            }

            infallible.insert(work_item);
            visited.insert(work_item);

            // Stop at local items to prevent over-linting
            if work_item.def_id().is_local() {
                continue;
            }

            for accessor in backward.get(&work_item).unwrap_or(&Vec::new()) {
                work_queue.push(accessor.node);
            }
        }

        for (accessor, accessees) in forward.iter() {
            // Don't report on non-local items
            if !accessor.def_id().is_local() {
                continue;
            }

            // Fast path
            if !infallible.contains(&accessor) {
                continue;
            }

            for item in accessees {
                let accessee = item.node;

                if !accessee.def_id().is_local() && infallible.contains(&accessee) {
                    cx.struct_span_lint(&INFALLIBLE_ALLOCATION, item.span, |diag| {
                        let is_generic = accessor.substs.non_erasable_generics().next().is_some();
                        let generic_note = if is_generic {
                            format!(
                                " when the caller is monomorphized as `{}`",
                                cx.tcx
                                    .def_path_str_with_substs(accessor.def_id(), accessor.substs)
                            )
                        } else {
                            String::new()
                        };

                        let accessee_path = cx
                            .tcx
                            .def_path_str_with_substs(accessee.def_id(), accessee.substs);

                        let mut diag = diag.build(&format!(
                            "`{}` can perform infallible allocation{}",
                            accessee_path, generic_note
                        ));

                        // For generic functions try to display a stacktrace until a non-generic one.
                        let mut caller = *accessor;
                        let mut visited = FxHashSet::default();
                        visited.insert(*accessor);
                        visited.insert(accessee);
                        while caller.substs.non_erasable_generics().next().is_some() {
                            let spanned_caller = match backward
                                .get(&caller)
                                .map(|x| &**x)
                                .unwrap_or(&[])
                                .iter()
                                .find(|x| !visited.contains(&x.node))
                            {
                                Some(v) => *v,
                                None => break,
                            };
                            caller = spanned_caller.node;
                            visited.insert(caller);

                            diag.span_note(
                                spanned_caller.span,
                                &format!(
                                    "which is called from `{}`",
                                    cx.tcx
                                        .def_path_str_with_substs(caller.def_id(), caller.substs)
                                ),
                            );
                        }

                        // Generate some help messages for why the function is determined to be infallible.
                        let mut msg: &str = &format!(
                            "`{}` is determined to be infallible because it",
                            accessee_path
                        );
                        let mut callee = accessee;
                        loop {
                            let callee_callee = match forward
                                .get(&callee)
                                .map(|x| &**x)
                                .unwrap_or(&[])
                                .iter()
                                .find(|x| {
                                    infallible.contains(&x.node) && !visited.contains(&x.node)
                                }) {
                                Some(v) => v,
                                None => break,
                            };
                            callee = callee_callee.node;
                            visited.insert(callee);

                            diag.span_note(
                                callee_callee.span,
                                &format!(
                                    "{} calls into `{}`",
                                    msg,
                                    cx.tcx
                                        .def_path_str_with_substs(callee.def_id(), callee.substs)
                                ),
                            );
                            msg = "which";
                        }

                        diag.note(&format!("{} may call alloc_error_handler", msg));
                        diag.emit();
                    });
                }
            }
        }
    }
}
