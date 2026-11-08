use super::*;

fn one(segment: Segment, path: Option<PathBuf>) -> Register {
    Register {
        entries: vec![RegisterEntry { segment, path }],
    }
}

#[test]
fn a_register_from_another_project_resolves_or_refuses() {
    let p = fixture();
    // The same file pooled here under a different id (asset 1 is
    // red.mp4 in the fixture): the register resolves by path.
    let mut other = p.clone();
    let (spare, _) = other.add_asset("/media/spare.mp4".into(), probe_report("5"));
    let reg = Register::of(
        &other,
        Segment::Clip(Clip {
            id: 7,
            asset: spare,
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
    );
    assert_eq!(
        reg.entries[0].path.as_deref(),
        Some(Path::new("/media/spare.mp4"))
    );
    // Not in this pool → refuse, loudly.
    assert!(matches!(
        reg.resolve(&p),
        Err(EditError::RegisterForeign(path)) if path.contains("spare.mp4")
    ));

    // A file this project *does* pool (under a different id) remaps.
    let mut here = p.clone();
    let (id, _) = here.add_asset("/media/other/red.mp4".into(), probe_report("10"));
    let entry = RegisterEntry {
        segment: Segment::Clip(Clip {
            id: 7,
            asset: 999, // the id it had over there
            asset_in: Seconds::from_integer(0),
            asset_out: Seconds::from_integer(2),
            speed: Seconds::from_integer(1),
            effects: Vec::new(),
        }),
        path: Some(PathBuf::from("/media/other/red.mp4")),
    };
    let resolved = entry.resolve(&here).unwrap();
    assert!(
        matches!(resolved, Segment::Clip(Clip { asset, .. }) if asset == id),
        "the asset id is remapped to this pool's: {resolved:?}"
    );
}

#[test]
fn the_register_round_trips_through_its_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nle.json");
    assert_eq!(load_register(&path), None, "no file, no register");
    let reg = one(
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        Some(PathBuf::from("/media/red.mp4")),
    );
    save_register(&path, Some(&reg)).unwrap();
    assert_eq!(load_register(&path), Some(reg));
    save_register(&path, None).unwrap();
    assert_eq!(load_register(&path), None, "clearing removes the file");
    assert!(!register_path(&path).exists());
}

#[test]
fn a_range_round_trips_and_pastes_every_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nile.json");
    let reg = Register::of_range(
        &fixture(),
        &[
            Segment::Clip(Clip {
                id: 1,
                asset: 1,
                asset_in: Seconds::from_integer(0),
                asset_out: Seconds::from_integer(2),
                speed: Seconds::from_integer(1),
                effects: Vec::new(),
            }),
            Segment::Gap(Gap {
                duration: Seconds::from_integer(1),
            }),
        ],
    );
    save_register(&path, Some(&reg)).unwrap();
    let loaded = load_register(&path).unwrap();
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(loaded.resolve_range(&fixture()).unwrap().len(), 2);
    assert!(matches!(
        loaded.resolve(&fixture()),
        Err(EditError::RangeRegister)
    ));
}

#[test]
fn an_old_adjacent_register_is_copied_into_the_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("nile.json");
    let reg = one(
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        None,
    );
    save_register(&project, Some(&reg)).unwrap();
    let new = register_path(&project);
    let legacy = dir.path().join("nile.clipboard.json");
    std::fs::copy(&new, &legacy).unwrap();
    std::fs::remove_file(&new).unwrap();

    assert_eq!(load_register(&project), Some(reg));
    assert!(
        new.exists(),
        "the old register is copied into .nile/<stem>/"
    );

    save_register(&project, None).unwrap();
    assert!(!new.exists());
    assert!(!legacy.exists(), "clearing removes the legacy copy too");
}

#[test]
fn the_read_only_loader_leaves_a_legacy_register_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("nile.json");
    let reg = one(
        Segment::Gap(Gap {
            duration: Seconds::from_integer(2),
        }),
        None,
    );
    save_register(&project, Some(&reg)).unwrap();
    let new = register_path(&project);
    let legacy = dir.path().join("nile.clipboard.json");
    std::fs::copy(&new, &legacy).unwrap();
    std::fs::remove_file(&new).unwrap();

    assert_eq!(load_register_without_migration(&project), Some(reg));
    assert!(
        !new.exists(),
        "a diagnostic read must not create the state-directory sidecar"
    );
    assert!(legacy.exists(), "the legacy sidecar is left alone");
}
