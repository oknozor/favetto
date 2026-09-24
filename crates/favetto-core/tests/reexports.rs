//! Guards the extraction contract: every pre-#217 `favetto_core` path still
//! resolves and re-exports the very same `favetto-wire` type.

use favetto_core::agent_state::{AgentUsage, InputReply};
use favetto_core::model::{Task, TaskStatus};
use favetto_core::rpc::{Frame, Request};
use favetto_core::tasks::{TaskVar, VarType};
use favetto_core::workflow::{WorkflowInspect, WorkflowState};

#[test]
fn task_var_reexport_is_the_wire_type() {
    let var = TaskVar {
        name: "v".into(),
        prompt: "p".into(),
        default: None,
        required: false,
        multiline: false,
        var_type: VarType::Int,
        choices: None,
    };
    let as_wire: favetto_wire::task_var::TaskVar = var.clone();
    assert_eq!(as_wire.coerce("3").unwrap(), serde_json::json!(3));
}

#[test]
fn envelope_reexport_serializes_as_before() {
    let frame = Frame::Request(Request {
        id: 1,
        method: "system.ping".into(),
        params: serde_json::json!({}),
    });
    let v = serde_json::to_value(&frame).unwrap();
    assert_eq!(v["type"], "request");
}

/// The re-exported paths name the same types as `favetto-wire`'s modules.
#[test]
fn reexported_types_are_the_wire_types() {
    fn assert_same_type<T>(_: std::marker::PhantomData<T>) {}
    assert_same_type::<favetto_wire::model::Task>(std::marker::PhantomData::<Task>);
    assert_same_type::<favetto_wire::model::TaskStatus>(std::marker::PhantomData::<TaskStatus>);
    assert_same_type::<favetto_wire::agent_state::AgentUsage>(
        std::marker::PhantomData::<AgentUsage>,
    );
    assert_same_type::<favetto_wire::agent_state::InputReply>(
        std::marker::PhantomData::<InputReply>,
    );
    assert_same_type::<favetto_wire::workflow::WorkflowInspect>(
        std::marker::PhantomData::<WorkflowInspect>,
    );
    assert_same_type::<favetto_wire::workflow::WorkflowState>(
        std::marker::PhantomData::<WorkflowState>,
    );
}
