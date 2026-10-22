use super::*;

#[test]
fn twins20_cache_isolation_content_first() {
    let e = super::tests::cache_document();
    let mut s = e.clone();
    s.instances[3]
        .attributes
        .insert("Changed".into(), serde_json::json!(true));
    let editor = Tree::new(&e);
    let studio = Tree::new(&s);
    let candidates = Candidates::default();
    let locks = vec![None, None, None, Some(3), None];
    let content = Matcher {
        budget: None,
        editor: &editor,
        studio: &studio,
        locked: &[],
        owners: &[],
        candidates: &candidates,
    };
    assert!(content.subtree(3, 3).is_none());
    let identity = Matcher {
        budget: None,
        locked: &locks,
        owners: &locks,
        ..content
    };
    assert!(
        identity.subtree(3, 3).is_some(),
        "identity cache cannot reuse content rejection"
    );
    assert!(content.subtree(3, 3).is_none());
}

#[test]
fn twins20_final_fence_rejects_reference_injectivity_and_lock_errors() {
    let d = super::tests::cache_document();
    let editor = Tree::new(&d);
    let studio = Tree::new(&d);
    let candidates = Candidates::default();
    let locked = vec![None, Some(1), None, None, None];
    let matcher = Matcher {
        budget: None,
        editor: &editor,
        studio: &studio,
        locked: &locked,
        owners: &locked,
        candidates: &candidates,
    };
    for mut partners in [
        vec![Some(0), Some(1), Some(2), Some(4), Some(3)],
        vec![Some(0), Some(1), Some(2), Some(3), Some(3)],
        vec![Some(0), Some(2), Some(1), Some(3), Some(4)],
    ] {
        let mut proven = vec![true; 5];
        take_recoveries();
        assert!(validate_resolution(&matcher, &mut partners, &mut proven));
        assert_eq!(take_recoveries(), 1);
        assert_eq!(partners, (0..5).map(Some).collect::<Vec<_>>());
        assert!(proven.iter().all(|&v| !v));
    }
}

#[test]
fn twins20_pending_reference_does_not_hide_a_decidable_failure() {
    let mut e = super::tests::cache_document();
    e.instances[3].properties.insert(
        "AAWait".into(),
        serde_json::json!({"_type":"Ref", "settingsId":"4"}),
    );
    let mut s = e.clone();
    s.instances[3].properties.insert(
        "Value".into(),
        serde_json::json!({"_type":"Ref", "settingsId":"2"}),
    );
    let editor = Tree::new(&e);
    let studio = Tree::new(&s);
    let candidates = Candidates::default();
    let matcher = Matcher {
        budget: None,
        editor: &editor,
        studio: &studio,
        locked: &[],
        owners: &[],
        candidates: &candidates,
    };
    let partners = vec![Some(0), Some(1), Some(2), Some(3), None];
    let pending = [4].into_iter().collect();
    let reads = RefCell::default();
    let mut partial = References::new(&matcher, &partners, &reads);
    partial.pending = Some(&pending);
    assert!(
        !partial.record_references_equal(3, 3),
        "AAWait is unknown, but Value is already false"
    );
    let studio = Tree::new(&e);
    let matcher = Matcher {
        studio: &studio,
        ..matcher
    };
    let mut partial = References::new(&matcher, &partners, &reads);
    partial.pending = Some(&pending);
    assert!(partial.record_references_equal(3, 3));
    let complete = References::new(&matcher, &partners, &reads);
    assert!(
        !complete.validates(&vec![(3, 3)]),
        "a deferred edge cannot pass the complete leaf proof"
    );
}

#[test]
fn twins20_exhaustion_never_becomes_cached_negative_evidence() {
    let d = super::tests::cache_document();
    let editor = Tree::new(&d);
    let studio = Tree::new(&d);
    let candidates = Candidates::default();
    let budget = search::Budget::new(1);
    let limited = Matcher {
        budget: Some(&budget),
        editor: &editor,
        studio: &studio,
        locked: &[],
        owners: &[],
        candidates: &candidates,
    };
    assert!(limited.subtree(0, 0).is_none());
    assert!(budget.exhausted.get());
    let complete = Matcher {
        budget: None,
        ..limited
    };
    assert_eq!(complete.subtree(0, 0).unwrap().len(), d.instances.len());
}
