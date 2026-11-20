//! Starlark DSL, static typechecker, and multi-file loader for `starchart`.

pub mod dsl;
pub mod loader;
pub mod types;

pub use dsl::{chart_dialect, chart_globals, register_dsl};
pub use loader::{
    ChartCompileError, CompiledChart, FsResolver, InMemoryResolver, SourceResolver,
    compile_starlark_file, compile_starlark_str, compile_with_resolver,
};
pub use types::{
    FrozenStarlarkMachine, StarlarkGuard, StarlarkMachine, StarlarkStateId, StarlarkStateNode,
    StarlarkStateSpace, StarlarkTransition, SymbolOrEnum,
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use starchart_ir::GuardRef;

    use super::*;

    fn workspace_example(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    #[test]
    fn order_workflow_example_typechecks_evaluates_and_freezes() {
        let path = workspace_example("examples/order_workflow.star");
        let compiled = compile_starlark_file(&path, None).expect("order_workflow.star should compile");

        assert_eq!(compiled.ir.id, "order_workflow");
        assert_eq!(compiled.ir.initial.as_str(), "validating");
        assert_eq!(compiled.guards.len(), 1);
        assert_eq!(compiled.reducers.len(), 1);
        assert_eq!(compiled.ir.validate(), Ok(()));
    }

    #[test]
    fn passing_raw_string_target_is_rejected_by_static_typechecker_before_eval() {
        let src = r#"
# If evaluation ran, `fail(...)` would execute; static typecheck must reject first!
fail("evaluation must not run")

idle = state_id("idle")
chart = machine(
    id = "bad_target",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(event = "GO", target = "charging"),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("bad_target.star", src, None)
            .expect_err("raw str target must fail static typecheck");
        assert!(
            matches!(err, ChartCompileError::Typecheck(_)),
            "expected static typecheck error, got: {err}"
        );
    }

    #[test]
    fn unknown_named_argument_fails_immediately_in_static_typecheck() {
        let src = r#"
idle = state_id("idle")
chart = machine(
    id = "bad_arg",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(evnt = "FOO", target = idle),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("bad_arg.star", src, None)
            .expect_err("unknown named argument must fail");
        assert!(
            matches!(err, ChartCompileError::Typecheck(_)),
            "expected static typecheck failure, got: {err}"
        );
    }

    #[test]
    fn dangling_state_id_fails_inside_machine_with_span_info() {
        let src = r#"
idle = state_id("idle")
ghost = state_id("ghost")

chart = machine(
    id = "dangling",
    initial = idle,
    states = {
        idle: state(
            on = [
                transition(event = "JUMP", target = ghost),
            ],
        ),
    },
)
"#;
        let err = compile_starlark_str("dangling.star", src, None)
            .expect_err("dangling StateId must fail inside machine(...)");
        let msg = err.to_string();
        assert!(msg.contains("dangling.star:5:"), "expected line/col span in error:\n{msg}");
        assert!(msg.contains("ghost"), "expected undeclared target name in error:\n{msg}");
    }

    #[test]
    fn multi_file_load_resolves_and_typechecks_across_modules() {
        let mut files = HashMap::new();
        files.insert(
            "events.star".to_owned(),
            r#"
OrderEvent = enum("START", "FINISH")
OrderCtx = record(retries = int)
"#
            .to_owned(),
        );

        let main_src = r#"
load("events.star", "OrderEvent", "OrderCtx")

S = state_space("idle", "done")

def bump(ctx: OrderCtx, event: dict) -> OrderCtx:
    return OrderCtx(retries = ctx.retries + 1)

chart = machine(
    id = "multi_file",
    initial = S.idle,
    context = OrderCtx(retries = 0),
    states = {
        S.idle: state(
            on = [
                internal_transition(event = OrderEvent.START, assign = bump),
                transition(event = OrderEvent.FINISH, target = S.done),
            ],
        ),
        S.done: final_state(),
    },
)
"#;
        let resolver = InMemoryResolver::new(files);
        let compiled = compile_with_resolver("main.star", main_src, "chart", &resolver)
            .expect("multi-file load with enum, record, state_space, and internal_transition should compile");
        assert_eq!(compiled.ir.id, "multi_file");
        assert!(
            compiled.context_record_type.is_some(),
            "expected OrderCtx RecordType to be captured from loaded module"
        );
    }

    #[test]
    fn guard_combinators_and_named_guards_compile_to_ir() {
        let src = r#"
S = state_space("locked", "unlocked")

def has_pin(ctx: dict, event: dict) -> bool:
    return event.get("pin") == ctx.get("pin")

chart = machine(
    id = "smart_lock",
    initial = S.locked,
    context = {"pin": 1234},
    states = {
        S.locked: state(
            on = [
                transition(
                    event = "UNLOCK",
                    target = S.unlocked,
                    guard = all_of(
                        not_(named_guard("IS_TAMPERED")),
                        any_of(has_pin, "HAS_MASTER_KEY"),
                    ),
                ),
            ],
        ),
        S.unlocked: final_state(),
    },
)
"#;
        let compiled = compile_starlark_str("smart_lock.star", src, None)
            .expect("guard combinators should compile");
        let guard = compiled.ir.states[&starchart_ir::StateId::from("locked")].on[0]
            .guard
            .as_ref()
            .expect("expected guard");
        assert_eq!(
            guard,
            &GuardRef::AllOf(vec![
                GuardRef::Not(Box::new(GuardRef::Named("IS_TAMPERED".into()))),
                GuardRef::AnyOf(vec![
                    GuardRef::StarlarkSlot(0),
                    GuardRef::Named("HAS_MASTER_KEY".into()),
                ]),
            ])
        );
    }
}
