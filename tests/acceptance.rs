//! Criteria a machine can actually check.
//!
//! The rule that shapes this: a criterion is data, not authority. Several of
//! these tests exist to prove it cannot become authority by being written
//! cleverly.

use timon::acceptance::{Criterion, Unusable, check, check_all, resolve};

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("NOTES.md"), "see GUIDE.md for details\n").unwrap();
    std::fs::write(dir.path().join("old.rs"), "fn deprecated_fn() {}\n").unwrap();
    dir
}

#[test]
fn a_criterion_is_true_or_false_by_inspection() {
    let dir = tree();
    let met = |criterion: &Criterion| check(dir.path(), criterion).unwrap().met;

    assert!(met(&Criterion::FileExists {
        path: "NOTES.md".into()
    }));
    assert!(!met(&Criterion::FileExists {
        path: "MISSING.md".into()
    }));
    assert!(met(&Criterion::FileAbsent {
        path: "MISSING.md".into()
    }));
    assert!(!met(&Criterion::FileAbsent {
        path: "NOTES.md".into()
    }));
    assert!(met(&Criterion::FileContains {
        path: "NOTES.md".into(),
        text: "GUIDE.md".into()
    }));
    assert!(!met(&Criterion::FileContains {
        path: "NOTES.md".into(),
        text: "absent text".into()
    }));
    assert!(met(&Criterion::FileOmits {
        path: "NOTES.md".into(),
        text: "deprecated_fn".into()
    }));
    assert!(!met(&Criterion::FileOmits {
        path: "old.rs".into(),
        text: "deprecated_fn".into()
    }));
}

#[test]
fn a_criterion_cannot_name_a_path_outside_the_tree() {
    // A criterion is data. A model that could point one at /etc or at the
    // developer's home would be a model reading whatever it liked.
    let dir = tree();
    for escape in ["/etc/passwd", "../outside.txt", "a/../../outside", "/"] {
        let error = resolve(dir.path(), escape).unwrap_err();
        assert_eq!(
            error,
            Unusable::PathEscapes {
                path: escape.into()
            },
            "{escape:?} should have been refused"
        );
        assert!(format!("{error}").contains("outside the tree"));
    }
    // And an ordinary nested path is fine.
    assert!(resolve(dir.path(), "docs/guide.md").is_ok());
}

#[test]
fn a_symlink_aimed_out_of_the_tree_is_refused_too() {
    // The other way this goes wrong: the path looks relative and innocent, and
    // what it resolves to is not.
    let dir = tree();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("link.txt"),
    )
    .unwrap();

    let error = resolve(dir.path(), "link.txt").unwrap_err();
    assert!(matches!(error, Unusable::PathEscapes { .. }), "{error:?}");
}

#[test]
fn an_unusable_criterion_is_not_a_failure() {
    // A criterion nobody could evaluate has established nothing. Calling it a
    // failure would send somebody fixing the wrong thing.
    let dir = tree();
    let report = check_all(
        dir.path(),
        &[
            Criterion::FileExists {
                path: "NOTES.md".into(),
            },
            Criterion::FileExists {
                path: "/etc/passwd".into(),
            },
        ],
    );
    assert_eq!(report.checked.len(), 1);
    assert_eq!(report.unusable.len(), 1);
    assert!(
        report.established(),
        "the one criterion that could be checked held"
    );
    assert!(
        report.summary().contains("could not be checked"),
        "{}",
        report.summary()
    );
}

#[test]
fn no_criteria_establishes_nothing() {
    let dir = tree();
    assert!(!check_all(dir.path(), &[]).established());

    // And a set where everything was unusable establishes nothing either.
    let report = check_all(
        dir.path(),
        &[Criterion::FileExists {
            path: "../escape".into(),
        }],
    );
    assert!(!report.established());
    assert!(report.checked.is_empty());
}

#[test]
fn a_summary_names_what_failed_rather_than_counting_it() {
    let dir = tree();
    let report = check_all(
        dir.path(),
        &[
            Criterion::FileExists {
                path: "NOTES.md".into(),
            },
            Criterion::FileExists {
                path: "GUIDE.md".into(),
            },
        ],
    );
    assert!(!report.established());
    let summary = report.summary();
    assert!(summary.contains("GUIDE.md exists"), "{summary}");
    assert!(summary.contains("not present"), "{summary}");
}

#[test]
fn a_missing_file_omits_everything_which_is_what_was_asked() {
    // Deleting the file satisfies "no longer contains", and reporting that as a
    // failure would push somebody to recreate a file in order to empty it.
    let dir = tree();
    let checked = check(
        dir.path(),
        &Criterion::FileOmits {
            path: "gone.rs".into(),
            text: "deprecated_fn".into(),
        },
    )
    .unwrap();
    assert!(checked.met);
    assert!(checked.detail.contains("does not exist"));
}

#[test]
fn an_empty_criterion_checks_nothing_and_says_so() {
    let dir = tree();
    assert_eq!(resolve(dir.path(), "   ").unwrap_err(), Unusable::Empty);
    assert_eq!(
        check(
            dir.path(),
            &Criterion::FileContains {
                path: "NOTES.md".into(),
                text: String::new()
            }
        )
        .unwrap_err(),
        Unusable::Empty
    );
}

#[test]
fn a_criterion_round_trips_through_the_json_a_planner_writes() {
    let json = r#"[
        {"kind":"file_exists","path":"GUIDE.md"},
        {"kind":"file_contains","path":"README.md","text":"GUIDE.md"},
        {"kind":"file_absent","path":"old.rs"},
        {"kind":"file_omits","path":"lib.rs","text":"deprecated_fn"}
    ]"#;
    let criteria: Vec<Criterion> = serde_json::from_str(json).expect("a planner's criteria");
    assert_eq!(criteria.len(), 4);
    assert_eq!(
        criteria[0],
        Criterion::FileExists {
            path: "GUIDE.md".into()
        }
    );
    assert!(criteria[1].describe().contains("README.md contains"));
}

#[test]
fn a_long_text_is_elided_in_the_description_but_not_in_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(200);
    std::fs::write(dir.path().join("f.txt"), &long).unwrap();
    let criterion = Criterion::FileContains {
        path: "f.txt".into(),
        text: long.clone(),
    };
    assert!(criterion.describe().len() < 80, "{}", criterion.describe());
    assert!(
        check(dir.path(), &criterion).unwrap().met,
        "the check uses the whole text"
    );
}
