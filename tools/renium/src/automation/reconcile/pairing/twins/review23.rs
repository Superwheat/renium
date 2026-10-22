use super::*;

#[test]
fn review23_final_publication_exhaustion_is_a_component_outcome() {
    let document = super::tests::cache_document();
    let editor = Tree::new(&document);
    let studio = Tree::new(&document);
    let candidates = Candidates::default();
    let locks = vec![None; document.instances.len()];
    let matcher = Matcher {
        budget: None,
        editor: &editor,
        studio: &studio,
        locked: &locks,
        owners: &locks,
        candidates: &candidates,
    };
    let mut partners = locks.clone();
    let mut proven = vec![false; locks.len()];
    let mut rejected = vec![false; locks.len()];
    let units = collect_units(&matcher, &mut partners, &mut proven);
    let reads = RefCell::default();
    let context = Context::new(&References::new(&matcher, &partners, &reads), &units);
    let mut completions = resolve_references(
        &matcher,
        &units,
        &context,
        &mut partners,
        &mut proven,
        &mut rejected,
    );
    assert_eq!(completions.len(), 1);
    assert!(proven[3] && proven[4]);
    completions[0].remaining = 0;
    take_recoveries();
    let publication = finish_publication(
        &matcher,
        &units,
        &context,
        completions,
        &mut partners,
        &mut proven,
        &mut rejected,
    );
    assert_eq!(publication.budget_conflicts.len(), 1);
    assert!(publication.valid);
    assert!(proven.iter().all(|&v| !v));
    assert!(rejected.iter().all(|&v| !v));
    assert!(publication.unchanged[3..].iter().all(|&v| !v));
    assert_eq!(partners, (0..locks.len()).map(Some).collect::<Vec<_>>());
    assert!(!fence_resolution(
        &matcher,
        &mut partners,
        &mut proven,
        publication.valid
    ));
    assert_eq!(take_recoveries(), 0);
}

#[test]
fn review23_nested_value_entries_are_charged_before_comparison() {
    let mut document = super::tests::cache_document();
    document.instances[3].attributes.insert(
        "Array".into(),
        Value::Array(vec![Value::Bool(true); 70_000]),
    );
    let editor = Tree::new(&document);
    let studio = Tree::new(&document);
    let candidates = Candidates::default();
    let budget = search::Budget::new(search::WORK_LIMIT);
    let matcher = Matcher {
        budget: Some(&budget),
        editor: &editor,
        studio: &studio,
        locked: &[],
        owners: &[],
        candidates: &candidates,
    };
    assert!(!matcher.record(3, 3));
    assert!(budget.exhausted.get());
    assert!(candidates.records.borrow().is_empty());
    assert!(
        Matcher {
            budget: None,
            ..matcher
        }
        .record(3, 3)
    );
}

#[test]
fn review23_reference_index_preserves_field_and_nested_locations() {
    let mut e = super::tests::cache_document();
    let reference = serde_json::json!({"_type":"Ref","settingsId":"1"});
    e.instances[3].attributes.insert(
        "Nested".into(),
        serde_json::json!({"items":[null,reference]}),
    );
    for changed in [
        serde_json::json!({"items":[reference,null]}),
        serde_json::json!({"other":[null,reference]}),
    ] {
        let mut s = e.clone();
        s.instances[3].attributes.insert("Nested".into(), changed);
        let editor = Tree::new(&e);
        let studio = Tree::new(&s);
        assert!(!reference_fields_equal(&editor, 3, &studio, 3, &|a, b| a == b));
    }
    let mut s = e.clone();
    let field = s.instances[3].attributes.remove("Nested").unwrap();
    s.instances[3].properties.insert("Nested".into(), field);
    assert!(!reference_fields_equal(
        &Tree::new(&e),
        3,
        &Tree::new(&s),
        3,
        &|a, b| a == b
    ));
}
