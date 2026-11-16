//! Workloads the property found or might miss, each pinned as a deterministic test

use super::*;

#[test]
pub(super) fn a_unit_that_destroys_makes_and_destroys_an_entity_it_found_names_it_as_destroyed_alone() {
    // The engine names an entity the commit found and left destroyed as destroyed alone, though the commit made it again
    // on the way; the model's union of the members' changes must say so too
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Destroy { id: "Task-1" },
            Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("y"))]) },
            Step::Destroy { id: "Task-1" },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_unit_that_grants_then_destroys_an_entity_names_the_role_on_the_destroyed_entity() {
    // A grant made before the destroy in the commit is the destroyed entity's `role` field, as the engine names it
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![Step::Grant { id: "Task-1", role: "r1" }, Step::Destroy { id: "Task-1" }],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_destroy_and_remake_in_a_nested_unit_drops_what_the_outer_unit_wrote_before_it() {
    // The engine names a destroyed-and-made entity with only what was done to the one made: the fields an enclosing unit
    // wrote before the nested unit's destroy are not among them
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Set { id: "Task-1", key: "c", value: json!("y") },
            Step::Unit {
                steps: vec![Step::Destroy { id: "Task-1" }, Step::Upsert { id: "Task-1", fields: BTreeMap::new() }],
                throws: false,
                durable: false,
            },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_rename_is_tracked_at_its_own_commit_though_the_builder_has_committed_since() {
    // A rename reports no sequence, and gives the rank index's builder work it commits by itself: by the time the
    // step is tracked the database's sequence may be the builder's, a commit past the one that stamped the ends
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
        Step::Link { id: "Task-2", kind: "refs", target: "Task-3" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    engine.db.wait_for_index_builds().unwrap();
    let quiet = engine.db.sequence().unwrap();
    let step = Step::RenameRelation { from: "refs", to: "tags" };
    let got = run(&engine.db, &step);
    engine.db.wait_for_index_builds().unwrap();
    assert_eq!(engine.db.sequence().unwrap(), quiet + 2, "the rename's commit, and the builder's after it");
    model.step(&step).expect("the model renames");
    track(&mut engine, &step, &got, model, quiet);
}

#[test]
pub(super) fn a_move_onto_the_target_a_relation_has_is_reported_as_a_change_and_as_no_move() {
    // The workload moves a relation onto any entity, the target it has among them: the engine reports the relation
    // changed and not moved. Alone, and after a link in the same commit, it names no ends it moved from; after a move
    // in the same commit, the ends that move took it from
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-2", kind: "refs", target: "Task-1" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let onto_its_own = Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-1", to: "Task-1" };
    let moved_then_onto_its_own = Step::Unit {
        steps: vec![
            Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-1", to: "Task-3" },
            Step::MoveRelation { id: "Task-2", kind: "refs", target: "Task-3", to: "Task-3" },
        ],
        throws: false,
        durable: false,
    };
    let linked_then_onto_its_own = Step::Unit {
        steps: vec![
            Step::Link { id: "Task-3", kind: "refs", target: "Task-1" },
            Step::MoveRelation { id: "Task-3", kind: "refs", target: "Task-1", to: "Task-1" },
        ],
        throws: false,
        durable: false,
    };
    for step in [onto_its_own, moved_then_onto_its_own, linked_then_onto_its_own] {
        reference.outcome = model.step(&step);
        assert_eq!(reference.outcome, Ok(()), "{step:?}");
        reference.model = model.clone();
        engine = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
            engine,
            &reference,
            Aimed { step, aimed: true },
        );
    }
}

#[test]
pub(super) fn two_moves_of_one_relation_in_one_commit_are_one_entry_with_its_first_ends() {
    // The engine names a relation moved twice in one commit once, changed, at its last ends and with the ends it began with
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-4", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::MoveRelation { id: "Task-1", kind: "refs", target: "Task-2", to: "Task-3" },
            Step::MoveRelation { id: "Task-1", kind: "refs", target: "Task-3", to: "Task-4" },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_rename_onto_an_edge_of_the_new_kind_with_the_same_ends_is_refused_by_both() {
    // Renaming refs to tags, where tags already joins the same two entities, would leave two edges with one identity: the
    // engine refuses it, and the model must refuse it the same way, moving nothing
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "tags", target: "Task-2" },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::RenameRelation { from: "refs", to: "tags" };
    reference.outcome = model.step(&step);
    assert!(reference.outcome.is_err(), "the model refuses the rename");
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_refused_rename_leaves_an_armed_refusal_for_the_next_commit_as_the_engine_does() {
    // The engine refuses a rename onto an edge of the new kind before its commit, so the refusal armed for the next commit
    // stays armed: the create after the failed rename is the one refused, by both
    let prelude = vec![
        Step::Create { id: "Task-1", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::Link { id: "Task-1", kind: "tags", target: "Task-2" },
        Step::Link { id: "Task-1", kind: "refs", target: "Task-2" },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let mut engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let steps = [
        Step::RefuseNext,
        Step::RenameRelation { from: "refs", to: "tags" },
        Step::Create { id: "Task-3", fields: BTreeMap::from([("a", json!("y"))]) },
    ];
    for step in steps {
        reference.outcome = model.step(&step);
        reference.model = model.clone();
        engine = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
            engine,
            &reference,
            Aimed { step, aimed: false },
        );
    }
    assert!(reference.outcome.is_err(), "the create after the failed rename is refused, as the armed refusal says");
}

#[test]
pub(super) fn a_field_rename_onto_a_name_the_task_holds_is_refused_by_both() {
    // A Task holding both c and spare would have one value replaced: the engine refuses the rename and the model must too,
    // moving nothing
    let prelude = vec![Step::Create { id: "Task-1", fields: BTreeMap::from([("c", json!(1)), ("spare", json!(2))]) }];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude creates the id once");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::RenameField { from: "c", to: "spare" };
    reference.outcome = model.step(&step);
    assert!(reference.outcome.is_err(), "the model refuses the rename");
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}

#[test]
pub(super) fn a_rekey_in_a_nested_unit_is_the_same_relation_as_an_earlier_rekey_of_it_in_the_commit() {
    // An outer unit rekeys an edge to the key it has, and a nested unit rekeys it again: one relation, one entry
    let prelude = vec![
        Step::Create { id: "Note-1", fields: BTreeMap::from([("a", json!("t"))]) },
        Step::Create { id: "Task-2", fields: BTreeMap::from([("a", json!("x"))]) },
        Step::LinkKeyed { id: "Note-1", target: "Task-2", n: 0 },
    ];
    let mut model = Model::default();
    for step in &prelude {
        model.step(step).expect("the prelude builds the state");
    }
    let mut reference = Reference { model: model.clone(), prelude, outcome: Ok(()) };
    let engine = <EngineMachine as proptest_state_machine::StateMachineTest>::init_test(&reference);
    let step = Step::Unit {
        steps: vec![
            Step::Rekey { id: "Note-1", target: "Task-2", from: 0, to: 0 },
            Step::Unit {
                steps: vec![Step::Rekey { id: "Note-1", target: "Task-2", from: 0, to: 1 }],
                throws: false,
                durable: false,
            },
        ],
        throws: false,
        durable: false,
    };
    reference.outcome = model.step(&step);
    reference.model = model;
    let _ = <EngineMachine as proptest_state_machine::StateMachineTest>::apply(
        engine,
        &reference,
        Aimed { step, aimed: true },
    );
}
