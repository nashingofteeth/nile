//! The action catalog's wire spelling. `nle-protocol` sends these values,
//! so the tagged JSON is a compatibility surface: a rename here changes the
//! protocol, not just a debug string.

use super::*;

/// Encode then decode through `serde_json`, asserting the value survives
/// exactly. JSON numbers are f64, so `PartialEq` on the action is the
/// check that matters.
fn round_trip(action: Action) -> serde_json::Value {
    let value = serde_json::to_value(&action).expect("serialize an action");
    let back: Action = serde_json::from_value(value.clone()).expect("deserialize an action");
    assert_eq!(back, action);
    value
}

#[test]
fn a_split_is_tagged_with_its_catalog_name() {
    let value = round_trip(Action::Split { track: 0, at: 1.5 });
    assert_eq!(
        value,
        serde_json::json!({"action": "split", "track": 0, "at": 1.5})
    );
}

#[test]
fn unit_actions_have_no_params() {
    assert_eq!(
        round_trip(Action::Undo),
        serde_json::json!({"action": "undo"})
    );
    assert_eq!(
        round_trip(Action::Redo),
        serde_json::json!({"action": "redo"})
    );
}

#[test]
fn every_target_spelling_round_trips() {
    assert_eq!(
        round_trip(Action::Delete {
            track: 0,
            target: Target::At(2.5),
            ripple: true,
        }),
        serde_json::json!({
            "action": "delete",
            "track": 0,
            "target": {"at": 2.5},
            "ripple": true
        })
    );
    assert_eq!(
        round_trip(Action::Yank {
            track: 1,
            target: Target::Clip(7),
        }),
        serde_json::json!({"action": "yank", "track": 1, "target": {"clip": 7}})
    );
    round_trip(Action::Extend {
        track: 0,
        target: Target::Segment(3),
        edge: Edge::Out,
        to: 4.0,
        ripple: false,
    });
}

#[test]
fn speed_keeps_the_exact_rational_rate() {
    let value = round_trip(Action::SetSpeed {
        track: 0,
        clip: 2,
        rate: Seconds::new(30000, 1001),
    });
    assert_eq!(
        value,
        serde_json::json!({
            "action": "set_speed",
            "track": 0,
            "clip": 2,
            "rate": "30000/1001"
        }),
        "a float rate would not survive the wire"
    );
}

#[test]
fn paste_carries_an_explicit_segment() {
    let segment = Segment::Clip(Clip {
        id: 9,
        asset: 1,
        asset_in: Seconds::from_integer(0),
        asset_out: Seconds::from_integer(3),
        speed: Seconds::from_integer(1),
        effects: Vec::new(),
    });
    let value = round_trip(Action::Paste {
        track: 0,
        at: 6.0,
        ripple: false,
        segment: Some(segment),
    });
    assert_eq!(value["action"], "paste");
    assert!(value["segment"].is_object(), "the clip is inline");
    assert_eq!(value["segment"]["Clip"]["asset_out"], "3/1");
}

#[test]
fn gap_and_transition_lengths_keep_their_units() {
    let gap = round_trip(Action::InsertGap {
        track: 0,
        at: 1.0,
        len: GapLen::Frames(12),
    });
    assert_eq!(gap["len"], serde_json::json!({"frames": 12}));

    let transition = round_trip(Action::Transition {
        track: 0,
        at: 2.0,
        kind: TransitionKind::FadeBlack,
        len: TransitionLen::Percent(50.0),
    });
    assert_eq!(transition["kind"], "FadeBlack");
    assert_eq!(transition["len"], serde_json::json!({"percent": 50.0}));
}

#[test]
fn effects_round_trip_through_the_catalog() {
    let value = round_trip(Action::AddEffect {
        track: 0,
        clip: 1,
        effect: Effect::Crop {
            left: 10.0,
            right: 5.0,
            top: 0.0,
            bottom: 0.0,
        },
        index: None,
    });
    assert_eq!(value["effect"]["Crop"]["left"], 10.0);

    round_trip(Action::RemoveEffect {
        track: 0,
        clip: 1,
        index: Some(2),
    });
}

#[test]
fn an_unknown_action_name_is_refused() {
    let err = serde_json::from_value::<Action>(serde_json::json!({"action": "rewind"}));
    assert!(err.is_err(), "unknown actions must not deserialize");
}

/// One sample per [`Action::slot`], in slot order. A new catalog variant
/// forces `SLOTS` up, and this list fails until the new shape is covered.
fn all_actions() -> Vec<Action> {
    let gap = Segment::Gap(Gap {
        duration: Seconds::from_integer(1),
    });
    vec![
        Action::TrimIn {
            track: 0,
            at: 1.0,
            ripple: false,
        },
        Action::TrimOut {
            track: 0,
            at: 1.0,
            ripple: true,
        },
        Action::Split { track: 0, at: 1.5 },
        Action::InsertGap {
            track: 0,
            at: 2.0,
            len: GapLen::Frames(12),
        },
        Action::Delete {
            track: 0,
            target: Target::At(1.0),
            ripple: false,
        },
        Action::Yank {
            track: 0,
            target: Target::Clip(1),
        },
        Action::Paste {
            track: 0,
            at: 3.0,
            ripple: false,
            segment: Some(gap),
        },
        Action::SetSpeed {
            track: 0,
            clip: 1,
            rate: Seconds::new(30000, 1001),
        },
        Action::Extend {
            track: 0,
            target: Target::Segment(1),
            edge: Edge::Out,
            to: 4.0,
            ripple: false,
        },
        Action::PurgeStumps { track: None },
        Action::TrackAdd {
            kind: TrackKind::Video,
        },
        Action::TrackMute { track: 0 },
        Action::TrackRm { track: 1 },
        Action::AssetRm { id: 1 },
        Action::Undo,
        Action::Redo,
        Action::Transition {
            track: 0,
            at: 2.0,
            kind: TransitionKind::Dissolve,
            len: TransitionLen::Frames(12),
        },
        Action::AddEffect {
            track: 0,
            clip: 1,
            effect: Effect::Eq {
                brightness: 0.0,
                contrast: 1.0,
                saturation: 1.0,
            },
            index: None,
        },
        Action::RemoveEffect {
            track: 0,
            clip: 1,
            index: Some(0),
        },
        Action::ClearEffects { track: 0, clip: 1 },
        Action::DeleteRange {
            track: 0,
            start: 0,
            end: 1,
        },
        Action::YankRange {
            track: 0,
            start: 0,
            end: 1,
        },
        Action::PasteRange { track: 0, at: 3.0 },
    ]
}

#[test]
fn every_catalog_slot_has_a_wire_form() {
    let actions = all_actions();
    assert_eq!(
        actions.len(),
        crate::action::SLOTS,
        "the sample list must cover every catalog slot"
    );
    for (slot, action) in actions.iter().enumerate() {
        assert_eq!(action.slot(), slot, "sample order follows Action::slot");
        let value = serde_json::to_value(action).expect("serialize an action");
        assert!(
            value.get("action").is_some_and(|tag| tag.is_string()),
            "every action carries its catalog tag: {value}"
        );
        let back: Action = serde_json::from_value(value).expect("deserialize an action");
        assert_eq!(&back, action);
    }
}

#[test]
fn omitted_optional_action_fields_default_to_none() {
    let paste: Action = serde_json::from_value(serde_json::json!({
        "action": "paste",
        "track": 0,
        "at": 1.0,
        "ripple": false
    }))
    .expect("paste without a segment");
    assert!(matches!(paste, Action::Paste { segment: None, .. }));

    let add: Action = serde_json::from_value(serde_json::json!({
        "action": "add_effect",
        "track": 0,
        "clip": 1,
        "effect": {"Eq": {"brightness": 0.0, "contrast": 1.0, "saturation": 1.0}}
    }))
    .expect("add_effect without an index");
    assert!(matches!(add, Action::AddEffect { index: None, .. }));

    let remove: Action = serde_json::from_value(serde_json::json!({
        "action": "remove_effect",
        "track": 0,
        "clip": 1
    }))
    .expect("remove_effect without an index");
    assert!(matches!(remove, Action::RemoveEffect { index: None, .. }));
}
