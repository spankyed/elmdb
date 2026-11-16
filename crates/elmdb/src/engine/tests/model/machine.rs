//! The engine run against the model: the state machine that drives both, and the property that runs it

use super::*;

pub(super) struct EngineMachine;

impl proptest_state_machine::StateMachineTest for EngineMachine {
    type SystemUnderTest = Engine;
    type Reference = ModelMachine;

    fn init_test(reference: &Reference) -> Engine {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut engine = Engine {
            db,
            _dir: dir,
            before: Model::default(),
            since: Since::default(),
            sequence: 0,
            readers: BTreeMap::from([("Task", Reader::default()), ("Note", Reader::default())]),
        };
        for step in &reference.prelude {
            // Read beside the builder, which a prelude runs with: no step of one is a rename, whose commit alone is
            // taken from this
            assert!(!matches!(step, Step::RenameRelation { .. } | Step::RenameField { .. }), "{step:?} in a prelude");
            let quiet = engine.db.sequence().expect("the sequence reads");
            let got = run(&engine.db, step);
            assert!(got.is_ok(), "the prelude's {step:?}: {got:?}");
            let mut after = engine.before.clone();
            after.step(step).expect("the prelude creates each id once");
            track(&mut engine, step, &got, after, quiet);
        }
        engine
    }

    fn apply(mut engine: Engine, reference: &Reference, transition: Aimed) -> Engine {
        // Read with the builder idle, so that the step's commit is the next: the builder commits what a step before
        // gave it to do, and is waited for here whether or not the invariants after that step were
        engine.db.wait_for_index_builds().expect("the builder's drain ends");
        let quiet = engine.db.sequence().expect("the sequence reads");
        let got = run(&engine.db, &transition.step);
        match (&got, &reference.outcome) {
            (Ok(_), Ok(())) => {}
            (Err(error), Err(reason)) => assert_eq!(
                error.code,
                code_of(reason),
                "{:?}: the engine refused with {error:?}, the model because {reason:?}",
                transition.step
            ),
            _ => panic!("{:?}: the engine said {got:?}, the model {:?}", transition.step, reference.outcome),
        }
        track(&mut engine, &transition.step, &got, reference.model.clone(), quiet);
        engine
    }

    fn check_invariants(engine: &Engine, reference: &Reference) {
        assert_eq!(state_of(&engine.db), model_state(&reference.model));
        assert_eq!(sorted_far_ends(derived_of(&engine.db)), derived_from(&reference.model));
        assert_eq!(plans_of(&engine.db), plans_from(&reference.model));
        // A registered field outlives its values, so what the model holds is all it must include
        engine
            .db
            .snapshot(|s| {
                for (id, entity) in &reference.model.entities {
                    for field in entity.fields.keys() {
                        let key = keys::field_name(crate::engine::ids::type_of(id), field);
                        assert!(s.get(&key).unwrap().is_some(), "{id}'s {field} isn't registered");
                    }
                }
                Ok(())
            })
            .unwrap();
        // The drain waited for, so no commit of its own takes a refusal the next step armed
        let live: Vec<String> =
            reference.model.entities.iter().filter(|(_, e)| !e.trashed).map(|(id, _)| id.clone()).collect();
        assert_eq!(RANKED.assert_within_bound(&engine.db), live, "the rank index's nodes");
        // Any state of the index is within the bound it reports, so that bound is what says the drain ran. Its report is
        // rounded up, a step at most about the tolerance
        let (_, error) = RANKED.indexed(&engine.db);
        assert!(error <= 2.0 * TOLERANCE, "the drain left a bound of {error}");
    }

    fn teardown(engine: Engine, _: Reference) {
        crate::failpoints::disarm_at(crate::failpoints::points::COMMIT, engine.db.path());
        declare_the_spares(&engine.db);
        let report = engine.db.check().unwrap();
        assert!(report.is_clean(), "check() after the workload: {report:?}");
    }
}

proptest_state_machine::prop_state_machine! {
    #![proptest_config(ProptestConfig { cases: 96, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_holds_what_the_model_holds_after_every_step(sequential 10..60 => EngineMachine);
}
