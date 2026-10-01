//! Provisioning the host-wide worker slots.
//!
//! The cross-account half — that a member can lock a slot and a non-member
//! cannot, and that neither can unlink one — needs real separate UIDs and root,
//! so `tests/shared-slots.sh` covers it. Nothing here establishes isolation.

use std::num::NonZeroU16;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use timon::worker::slots::{SlotMode, SlotPool, provision};

fn slots(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

fn inodes(dir: &Path) -> Vec<u64> {
    let mut found: Vec<u64> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("slot-"))
        .map(|e| e.metadata().unwrap().ino())
        .collect();
    found.sort();
    found
}

#[test]
fn provisioning_creates_the_directory_and_every_slot() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");

    let report = provision(&shared, slots(4), None, 0o750, 0o660).unwrap();

    assert_eq!(report.slots.len(), 4);
    assert!(report.slots.iter().all(|s| s.created));
    assert_eq!(report.dir_mode, 0o750);
    assert!(report.slots.iter().all(|s| s.mode == 0o660));
    assert_eq!(inodes(&shared).len(), 4);
}

#[test]
fn re_provisioning_leaves_every_existing_slot_file_exactly_where_it_was() {
    // The property the whole mechanism rests on. If re-provisioning replaced a
    // file, a launcher already holding the old inode would keep its lock while
    // another took the new one, and the host limit would quietly double.
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(3), None, 0o750, 0o660).unwrap();
    let before = inodes(&shared);

    let second = provision(&shared, slots(3), None, 0o750, 0o660).unwrap();

    assert_eq!(inodes(&shared), before, "the files must be the same files");
    assert!(
        second.slots.iter().all(|s| !s.created),
        "a second run creates nothing"
    );
}

#[test]
fn re_provisioning_does_not_disturb_a_slot_somebody_is_holding() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();
    let pool = SlotPool::new(&shared, slots(2), SlotMode::Provisioned);

    let held = pool.try_acquire().unwrap();
    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();

    // Still exactly one slot free, so the held one was neither released nor
    // replaced by a file a second caller could lock.
    let second = pool.try_acquire().unwrap();
    assert!(pool.try_acquire().is_err(), "the limit still holds at two");
    drop(second);
    drop(held);
}

#[test]
fn growing_the_pool_adds_slots_without_touching_the_old_ones() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();
    let before = inodes(&shared);

    let grown = provision(&shared, slots(5), None, 0o750, 0o660).unwrap();

    assert_eq!(grown.slots.iter().filter(|s| s.created).count(), 3);
    let after = inodes(&shared);
    assert!(
        before.iter().all(|ino| after.contains(ino)),
        "the original files survive a grow"
    );
}

#[test]
fn shrinking_leaves_the_extra_files_alone_and_says_so() {
    // Removing a slot file while a launcher holds its lock would hand that slot
    // to somebody else, so a shrink is reported rather than applied.
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(5), None, 0o750, 0o660).unwrap();

    let shrunk = provision(&shared, slots(2), None, 0o750, 0o660).unwrap();

    assert_eq!(shrunk.slots.len(), 2);
    assert_eq!(
        shrunk.extra_left_in_place.len(),
        3,
        "the operator has to be told they are still there"
    );
    assert_eq!(inodes(&shared).len(), 5, "and nothing was removed");
}

#[test]
fn the_directory_is_not_group_writable_so_members_cannot_add_or_remove_slots() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();

    let mode = std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o020,
        0,
        "group write on the directory would allow unlink"
    );
    assert_eq!(mode & 0o007, 0, "and others must not reach it at all");
}

#[test]
fn a_half_finished_earlier_run_is_corrected_rather_than_left_unlockable() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();
    let slot = shared.join("slot-000.lock");
    let before = std::fs::metadata(&slot).unwrap().ino();
    // A file left with a mode nobody but its owner can lock.
    std::fs::set_permissions(&slot, std::fs::Permissions::from_mode(0o600)).unwrap();

    provision(&shared, slots(2), None, 0o750, 0o660).unwrap();

    assert_eq!(
        std::fs::metadata(&slot).unwrap().permissions().mode() & 0o777,
        0o660,
        "the mode is put right"
    );
    assert_eq!(
        std::fs::metadata(&slot).unwrap().ino(),
        before,
        "without replacing the file to do it"
    );
}

#[test]
fn a_provisioned_pool_never_creates_a_missing_slot_itself() {
    // The shared directory is operator-owned. A launcher that created a missing
    // file would add a slot beyond the host limit, so it must fail instead.
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");
    provision(&shared, slots(1), None, 0o750, 0o660).unwrap();
    let pool = SlotPool::new(&shared, slots(2), SlotMode::Provisioned);

    let first = pool.try_acquire().unwrap();
    let second = pool.try_acquire();

    assert!(
        second.is_err(),
        "the second slot does not exist and is not created"
    );
    assert!(!shared.join("slot-001.lock").exists());
    drop(first);
}

#[test]
fn an_unknown_group_is_refused_before_anything_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("slots");

    let result = provision(&shared, slots(2), Some("no-such-group-here"), 0o750, 0o660);

    assert!(result.is_err());
    assert!(!shared.exists(), "nothing is left half-made");
}

/// Per-user fairness: the host limit stops the machine being overloaded and
/// says nothing about whose work is on it. Codex's review asked for this and
/// `TODO.md` recorded it as not built.
mod fairness {
    use std::num::NonZeroU16;
    use timon::worker::slots::{SlotError, SlotMode, SlotPool};

    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;

    fn pool(dir: &std::path::Path, limit: u16, per_user: u16) -> SlotPool {
        SlotPool::new(
            dir,
            NonZeroU16::new(limit).unwrap(),
            SlotMode::CreateMissing,
        )
        .per_user(NonZeroU16::new(per_user).unwrap())
    }

    #[test]
    fn one_user_cannot_take_every_slot() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 4, 2);

        let _first = pool.try_acquire_as(ALICE).expect("first");
        let _second = pool.try_acquire_as(ALICE).expect("second");

        // Two host slots are still free, and Alice may not have them.
        match pool.try_acquire_as(ALICE) {
            Err(SlotError::YoursFull { uid, cap }) => {
                assert_eq!((uid, cap), (ALICE, 2));
            }
            other => panic!("expected Alice to be at her cap, got {other:?}"),
        }
    }

    /// The point of the whole thing: the slots Alice was refused are available
    /// to somebody else, rather than being idle because she asked first.
    #[test]
    fn slots_refused_to_one_user_are_still_there_for_another() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 4, 2);

        let _a1 = pool.try_acquire_as(ALICE).expect("alice 1");
        let _a2 = pool.try_acquire_as(ALICE).expect("alice 2");
        assert!(pool.try_acquire_as(ALICE).is_err(), "alice is capped");

        let _b1 = pool.try_acquire_as(BOB).expect("bob 1");
        let _b2 = pool.try_acquire_as(BOB).expect("bob 2");
        assert!(pool.try_acquire_as(BOB).is_err(), "bob is capped too");
    }

    /// `Full` and `YoursFull` are different situations and must not be confused:
    /// one says wait, the other says your own work is the reason.
    #[test]
    fn a_busy_host_is_reported_differently_from_a_capped_user() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 2, 2);

        let _a1 = pool.try_acquire_as(ALICE).expect("alice 1");
        let _a2 = pool.try_acquire_as(ALICE).expect("alice 2");

        // Alice holds both host slots, so Bob meets a full host, not a cap.
        match pool.try_acquire_as(BOB) {
            Err(SlotError::Full) => {}
            other => panic!("expected a full host for Bob, got {other:?}"),
        }
        assert!(
            format!("{}", SlotError::Full).contains("all worker slots"),
            "the host message does not blame the caller"
        );
        let mine = SlotError::YoursFull { uid: ALICE, cap: 2 };
        assert!(
            format!("{mine}").contains("other slots may be free"),
            "the per-user message does not say the host may have room"
        );
    }

    /// Releasing a lease returns both slots. Returning the host slot but not the
    /// user's own would leave a caller at their cap holding nothing — a slow
    /// leak into permanent refusal.
    #[test]
    fn releasing_a_lease_returns_the_users_own_slot_too() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 4, 1);

        let first = pool.try_acquire_as(ALICE).expect("first");
        assert_eq!(first.user_index(), Some(0));
        assert!(pool.try_acquire_as(ALICE).is_err(), "capped at one");

        drop(first);
        let again = pool.try_acquire_as(ALICE).expect("the cap was released");
        assert_eq!(again.user_index(), Some(0), "the same user slot came back");
    }

    #[test]
    fn without_a_cap_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SlotPool::new(
            dir.path(),
            NonZeroU16::new(3).unwrap(),
            SlotMode::CreateMissing,
        );

        let held: Vec<_> = (0..3)
            .map(|i| {
                pool.try_acquire_as(ALICE)
                    .unwrap_or_else(|e| panic!("slot {i}: {e}"))
            })
            .collect();
        assert_eq!(held.len(), 3, "one uid may still fill the host");
        assert!(held.iter().all(|lease| lease.user_index().is_none()));
        assert!(matches!(pool.try_acquire_as(ALICE), Err(SlotError::Full)));
    }

    /// A cap above the host limit is allowed and simply never binds. An operator
    /// sizing a host should not have to work out which of two numbers wins.
    #[test]
    fn a_cap_above_the_host_limit_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(dir.path(), 2, 50);

        let _a = pool.try_acquire_as(ALICE).expect("first");
        let _b = pool.try_acquire_as(ALICE).expect("second");
        assert!(matches!(pool.try_acquire_as(ALICE), Err(SlotError::Full)));
    }
}
