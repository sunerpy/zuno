//! Attention notification and sound-cue tests.

use super::*;
use crate::config::TuiConfig;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Fakes
//
// Neither channel may touch hardware: `cargo test` has no display server and no
// audio device, and this crate must not acquire a dependency on either in order
// to be testable. Both fakes record what they were asked to do.
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RecordingNotifier {
    shown: Mutex<Vec<(String, String)>>,
    answer: bool,
}

impl RecordingNotifier {
    fn new(answer: bool) -> Self {
        Self {
            shown: Mutex::new(Vec::new()),
            answer,
        }
    }

    fn shown(&self) -> Vec<(String, String)> {
        self.shown.lock().expect("uncontended").clone()
    }

    fn count(&self) -> usize {
        self.shown().len()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, message: &str) -> bool {
        self.shown
            .lock()
            .expect("uncontended")
            .push((title.to_owned(), message.to_owned()));
        self.answer
    }
}

#[derive(Debug)]
struct RecordingPlayer {
    played: Mutex<Vec<(PathBuf, f64)>>,
    answer: bool,
}

impl RecordingPlayer {
    fn new(answer: bool) -> Self {
        Self {
            played: Mutex::new(Vec::new()),
            answer,
        }
    }

    fn played(&self) -> Vec<(PathBuf, f64)> {
        self.played.lock().expect("uncontended").clone()
    }

    fn count(&self) -> usize {
        self.played().len()
    }
}

impl SoundPlayer for RecordingPlayer {
    fn play(&self, file: &Path, volume: f64) -> bool {
        self.played
            .lock()
            .expect("uncontended")
            .push((file.to_owned(), volume));
        self.answer
    }
}

/// A pack that fills every slot, standing in for whatever a user supplies.
fn full_pack(id: &str) -> SoundPack {
    SoundName::ALL
        .into_iter()
        .fold(SoundPack::new(id).with_name("Test Pack"), |pack, name| {
            pack.with_sound(name, format!("/packs/{id}/{name}.mp3"))
        })
}

/// A host that is switched on, has a filled pack, and knows the terminal is blurred.
///
/// Blurred rather than unknown because the notification channel is
/// `when: "blurred"` for every class, so an unknown focus would decline it and the
/// happy path would silently be the sad path.
fn ready() -> (Attention, Arc<RecordingNotifier>, Arc<RecordingPlayer>) {
    let notifier = Arc::new(RecordingNotifier::new(true));
    let player = Arc::new(RecordingPlayer::new(true));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "test.pack".to_owned(),
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone())
    .with_player(player.clone());
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);
    (attention, notifier, player)
}

// ---------------------------------------------------------------------------
// The five event classes
// ---------------------------------------------------------------------------

#[test]
fn attention_each_event_class_resolves_to_its_configured_cue() {
    // The acceptance table. Columns: the class, the sound slot it asks for, the
    // message upstream's plugin passes, and whether the notification channel is
    // available to it at all. Sourced from
    // `packages/tui/src/feature-plugins/system/notifications.ts:35-86`.
    let expected: [(EventClass, SoundName, &str, bool); 5] = [
        (
            EventClass::PermissionNeeded,
            SoundName::Permission,
            "Permission needs input",
            true,
        ),
        (
            EventClass::QuestionAsked,
            SoundName::Question,
            "Question needs input",
            true,
        ),
        (EventClass::Error, SoundName::Error, "Session error", true),
        (EventClass::Done, SoundName::Done, "Session done", true),
        (
            EventClass::SubagentDone,
            SoundName::SubagentDone,
            "Session done",
            false,
        ),
    ];
    assert_eq!(
        expected.len(),
        EventClass::ALL.len(),
        "every event class needs a row"
    );

    for (class, sound, message, notifies) in expected {
        let cue = class.cue();
        assert_eq!(cue.class, class);
        assert_eq!(cue.sound, sound, "{class:?} asks for the wrong slot");
        assert_eq!(cue.message, message, "{class:?} has the wrong message");
        assert_eq!(
            cue.notification_when.is_some(),
            notifies,
            "{class:?} has the wrong notification availability"
        );
        assert_eq!(
            cue.sound_when,
            When::Always,
            "audio is unconditional for every class (`notifications.ts:16`)"
        );

        let (mut attention, notifier, player) = ready();
        let outcome = attention.notify(&AttentionRequest::new(class));

        assert!(outcome.sound, "{class:?} must play its cue: {outcome:?}");
        assert_eq!(
            player.played(),
            vec![(
                PathBuf::from(format!("/packs/test.pack/{sound}.mp3")),
                DEFAULT_VOLUME
            )],
            "{class:?} must play the file its slot names"
        );
        assert_eq!(
            outcome.notification, notifies,
            "{class:?} notification: {outcome:?}"
        );
        assert_eq!(
            notifier.shown(),
            if notifies {
                vec![(DEFAULT_TITLE.to_owned(), message.to_owned())]
            } else {
                Vec::new()
            },
            "{class:?} notification body"
        );
        assert!(outcome.ok(), "{class:?} delivered nothing");
        assert_eq!(outcome.skipped, None, "{class:?}: {outcome:?}");
        assert_eq!(outcome.diagnostics, Vec::new(), "{class:?}: {outcome:?}");
    }
}

#[test]
fn attention_a_subagent_cue_is_sound_only_by_construction() {
    // Upstream passes `notification: false` for any subagent event
    // (`notifications.ts:15`), so this is not a config knob that could be turned
    // back on: a desktop notification per finished subagent would be the loudest
    // thing in the app.
    assert!(EventClass::SubagentDone.is_subagent());
    assert!(EventClass::SubagentDone.cue().notification_when.is_none());
    for class in EventClass::ALL {
        assert_eq!(
            class.is_subagent(),
            class == EventClass::SubagentDone,
            "{class:?}"
        );
    }

    let (mut attention, notifier, player) = ready();
    let outcome = attention.notify(&AttentionRequest::new(EventClass::SubagentDone));
    assert!(!outcome.notification);
    assert!(outcome.sound);
    assert!(outcome.ok());
    assert_eq!(notifier.count(), 0);
    assert_eq!(player.count(), 1);
}

#[test]
fn attention_the_error_class_carries_the_message_the_error_produced() {
    // `sessionErrorMessage` (`notifications.ts:20-27`) distinguishes three cases;
    // the class default is its fallback and the caller supplies the other two.
    let (mut attention, notifier, _player) = ready();
    let outcome = attention
        .notify(&AttentionRequest::new(EventClass::Error).with_message("Model stopped responding"));
    assert!(outcome.notification);
    assert_eq!(
        notifier.shown(),
        vec![(
            DEFAULT_TITLE.to_owned(),
            "Model stopped responding".to_owned()
        )]
    );
}

#[test]
fn attention_a_session_title_replaces_the_default_one() {
    let (mut attention, notifier, _player) = ready();
    attention.notify(&AttentionRequest::new(EventClass::Done).with_title("Refactor the parser"));
    assert_eq!(
        notifier.shown(),
        vec![("Refactor the parser".to_owned(), "Session done".to_owned())]
    );
}

// ---------------------------------------------------------------------------
// enabled: false
// ---------------------------------------------------------------------------

#[test]
fn attention_disabled_produces_no_cue_at_all() {
    for class in EventClass::ALL {
        let notifier = Arc::new(RecordingNotifier::new(true));
        let player = Arc::new(RecordingPlayer::new(true));
        // Everything else says yes: both channels on, a full pack, a blurred
        // terminal. Only `enabled` is false, so nothing but the master switch can
        // account for the silence.
        let mut attention = Attention::new(ResolvedAttention {
            enabled: false,
            notifications: true,
            sound: true,
            sound_pack: "test.pack".to_owned(),
            ..ResolvedAttention::default()
        })
        .with_notifier(notifier.clone())
        .with_player(player.clone());
        assert!(attention.register_pack(full_pack("test.pack")));
        attention.set_focus(FocusState::Blurred);

        let outcome = attention.notify(&AttentionRequest::new(class));

        assert!(!outcome.notification, "{class:?}: {outcome:?}");
        assert!(!outcome.sound, "{class:?}: {outcome:?}");
        assert!(!outcome.ok(), "{class:?}: {outcome:?}");
        assert_eq!(
            outcome.skipped,
            Some(SkipReason::AttentionDisabled),
            "{class:?}: {outcome:?}"
        );
        assert_eq!(
            (notifier.count(), player.count()),
            (0, 0),
            "{class:?} reached a channel while disabled"
        );
    }
}

#[test]
fn attention_is_disabled_by_default() {
    // `config/index.tsx:103` — absent means off. A tool that starts making noise
    // without being asked is a tool people turn off entirely.
    assert!(!ResolvedAttention::default().enabled);
    let (settings, diagnostics) = AttentionSettings::default().resolve();
    assert!(!settings.enabled);
    assert!(settings.notifications, "on, but gated behind `enabled`");
    assert!(settings.sound, "on, but gated behind `enabled`");
    assert_eq!(settings.volume, DEFAULT_VOLUME);
    assert_eq!(settings.sound_pack, DEFAULT_PACK_ID);
    assert_eq!(diagnostics, Vec::new());
}

// ---------------------------------------------------------------------------
// The enabled / notifications / sound matrix
// ---------------------------------------------------------------------------

#[test]
fn attention_the_channel_matrix_is_two_independent_switches_under_one_master() {
    // Columns: enabled, notifications, sound -> notification fired, sound fired.
    let matrix: [(bool, bool, bool, bool, bool); 8] = [
        (false, false, false, false, false),
        (false, false, true, false, false),
        (false, true, false, false, false),
        (false, true, true, false, false),
        (true, false, false, false, false),
        (true, false, true, false, true),
        (true, true, false, true, false),
        (true, true, true, true, true),
    ];

    for (enabled, notifications, sound, want_notification, want_sound) in matrix {
        let label = format!("enabled={enabled} notifications={notifications} sound={sound}");
        let notifier = Arc::new(RecordingNotifier::new(true));
        let player = Arc::new(RecordingPlayer::new(true));
        let mut attention = Attention::new(ResolvedAttention {
            enabled,
            notifications,
            sound,
            sound_pack: "test.pack".to_owned(),
            ..ResolvedAttention::default()
        })
        .with_notifier(notifier.clone())
        .with_player(player.clone());
        assert!(attention.register_pack(full_pack("test.pack")));
        attention.set_focus(FocusState::Blurred);

        let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));

        assert_eq!(
            outcome.notification, want_notification,
            "{label}: {outcome:?}"
        );
        assert_eq!(outcome.sound, want_sound, "{label}: {outcome:?}");
        assert_eq!(
            outcome.ok(),
            want_notification || want_sound,
            "{label}: {outcome:?}"
        );
        assert_eq!(
            notifier.count(),
            usize::from(want_notification),
            "{label}: notifier calls"
        );
        assert_eq!(
            player.count(),
            usize::from(want_sound),
            "{label}: player calls"
        );
    }
}

// ---------------------------------------------------------------------------
// A missing sound pack
// ---------------------------------------------------------------------------

#[test]
fn attention_a_missing_sound_pack_degrades_to_notification_only_with_a_diagnostic() {
    let notifier = Arc::new(RecordingNotifier::new(true));
    let player = Arc::new(RecordingPlayer::new(true));
    // The default `sound_pack`. It is registered and it is empty, because this
    // crate ships no audio — see the module header.
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone())
    .with_player(player.clone());
    attention.set_focus(FocusState::Blurred);
    assert_eq!(attention.active_pack(), DEFAULT_PACK_ID);
    assert!(attention.sound_candidates(SoundName::Done).is_empty());

    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));

    assert!(outcome.notification, "the notification half must survive");
    assert!(!outcome.sound, "there is no file to play");
    assert!(outcome.ok(), "notification-only is still a delivered cue");
    assert_eq!(
        outcome.skipped, None,
        "a missing sound is not a skipped cue: {outcome:?}"
    );
    assert_eq!(
        outcome.diagnostics,
        vec![AttentionDiagnostic::MissingSound {
            pack: DEFAULT_PACK_ID.to_owned(),
            name: SoundName::Done,
        }]
    );
    assert_eq!(
        outcome.diagnostics[0].to_string(),
        "sound pack \"opencode.default\" has no done sound and none is configured under `attention.sounds.done`; notifying without audio"
    );
    assert_eq!(
        player.count(),
        0,
        "with no candidate there is nothing to hand a player"
    );
}

#[test]
fn attention_an_unregistered_pack_name_is_reported_and_falls_back() {
    let notifier = Arc::new(RecordingNotifier::new(true));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "acme.chimes".to_owned(),
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone());
    attention.set_focus(FocusState::Blurred);

    let outcome = attention.notify(&AttentionRequest::new(EventClass::Error));

    assert!(outcome.notification);
    assert!(!outcome.sound);
    assert_eq!(
        outcome.diagnostics,
        vec![
            AttentionDiagnostic::UnknownSoundPack {
                pack: "acme.chimes".to_owned(),
                available: vec![DEFAULT_PACK_ID.to_owned()],
            },
            AttentionDiagnostic::MissingSound {
                pack: "acme.chimes".to_owned(),
                name: SoundName::Error,
            },
        ],
        "both the unknown name and the resulting silence are named"
    );
    assert_eq!(
        outcome.diagnostics[0].to_string(),
        "no sound pack named \"acme.chimes\" is registered (available: \"opencode.default\"); falling back to \"opencode.default\""
    );
}

#[test]
fn attention_a_pack_that_cannot_be_played_is_reported_with_its_candidates() {
    let notifier = Arc::new(RecordingNotifier::new(true));
    // The player that ships: it plays nothing, so a configured file still degrades
    // to notification-only, and says so rather than pretending.
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "test.pack".to_owned(),
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone())
    .with_player(Arc::new(SilentPlayer));
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);

    let outcome = attention.notify(&AttentionRequest::new(EventClass::QuestionAsked));

    assert!(outcome.notification);
    assert!(!outcome.sound);
    assert_eq!(
        outcome.diagnostics,
        vec![AttentionDiagnostic::SoundUnplayable {
            name: SoundName::Question,
            candidates: vec![PathBuf::from("/packs/test.pack/question.mp3")],
        }]
    );
    assert!(
        outcome.diagnostics[0]
            .to_string()
            .contains("/packs/test.pack/question.mp3"),
        "{}",
        outcome.diagnostics[0]
    );
}

// ---------------------------------------------------------------------------
// Candidate order and packs
// ---------------------------------------------------------------------------

#[test]
fn attention_a_per_slot_override_beats_the_active_pack() {
    // `soundCandidates` (`attention.ts:146-150`): the user's own path first, then
    // the pack, then the built-in. Deduplicated, so naming the same file twice
    // does not make it two candidates.
    let player = Arc::new(RecordingPlayer::new(true));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "test.pack".to_owned(),
        sounds: BTreeMap::from([(SoundName::Done, PathBuf::from("/home/me/ding.mp3"))]),
        ..ResolvedAttention::default()
    })
    .with_player(player.clone());
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);

    assert_eq!(
        attention.sound_candidates(SoundName::Done),
        vec![
            PathBuf::from("/home/me/ding.mp3"),
            PathBuf::from("/packs/test.pack/done.mp3"),
        ]
    );
    assert_eq!(
        attention.sound_candidates(SoundName::Permission),
        vec![PathBuf::from("/packs/test.pack/permission.mp3")],
        "a slot with no override falls through to the pack"
    );

    attention.notify(&AttentionRequest::new(EventClass::Done));
    assert_eq!(
        player.played(),
        vec![(PathBuf::from("/home/me/ding.mp3"), DEFAULT_VOLUME)],
        "the first candidate that plays wins"
    );
}

#[test]
fn attention_the_next_candidate_is_tried_when_one_will_not_play() {
    #[derive(Debug)]
    struct OnlyPackFiles(AtomicUsize);
    impl SoundPlayer for OnlyPackFiles {
        fn play(&self, file: &Path, _volume: f64) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            file.starts_with("/packs")
        }
    }

    let player = Arc::new(OnlyPackFiles(AtomicUsize::new(0)));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "test.pack".to_owned(),
        sounds: BTreeMap::from([(SoundName::Done, PathBuf::from("/home/me/deleted.mp3"))]),
        ..ResolvedAttention::default()
    })
    .with_player(player.clone());
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);

    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert!(outcome.sound, "the pack's file answered: {outcome:?}");
    assert_eq!(player.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        outcome.diagnostics,
        Vec::new(),
        "a candidate that did not play is not a diagnostic when a later one did"
    );
}

#[test]
fn attention_the_builtin_pack_is_registered_and_empty() {
    let pack = builtin_pack();
    assert_eq!(pack.id(), DEFAULT_PACK_ID);
    assert_eq!(pack.name(), Some("OpenCode Default"));
    assert!(
        pack.is_empty(),
        "this crate ships no audio; see the module header for why and for how to supply a pack"
    );
    for name in SoundName::ALL {
        assert_eq!(pack.sound(name), None, "{name}");
    }

    let attention = Attention::new(ResolvedAttention::default());
    assert_eq!(
        attention.packs(),
        vec![SoundPackInfo {
            id: DEFAULT_PACK_ID.to_owned(),
            name: Some("OpenCode Default".to_owned()),
            active: true,
            builtin: true,
        }]
    );
}

#[test]
fn attention_a_registered_pack_can_be_activated_listed_and_removed() {
    let mut attention = Attention::new(ResolvedAttention::default());
    assert!(
        !attention.activate_pack("acme.chimes"),
        "not registered yet"
    );
    assert!(attention.register_pack(full_pack("acme.chimes")));
    assert!(attention.activate_pack("acme.chimes"));
    assert_eq!(attention.active_pack(), "acme.chimes");

    let listed = attention.packs();
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|pack| pack.id == "acme.chimes" && pack.active && !pack.builtin)
    );
    assert!(
        listed
            .iter()
            .any(|pack| pack.id == DEFAULT_PACK_ID && !pack.active && pack.builtin)
    );

    assert!(
        !attention.unregister_pack(DEFAULT_PACK_ID),
        "the built-in pack is what a diagnostic falls back to naming"
    );
    assert!(attention.unregister_pack("acme.chimes"));
    assert!(!attention.unregister_pack("acme.chimes"));
}

#[test]
fn attention_a_pack_without_an_id_is_refused() {
    // A pack nothing can name cannot be selected, so registering it would create
    // an entry no configuration could reach (`attention.ts:91-94`).
    let mut attention = Attention::new(ResolvedAttention::default());
    assert!(!attention.register_pack(SoundPack::new("   ")));
    assert_eq!(attention.packs().len(), 1);

    assert!(
        attention.register_pack(
            SoundPack::new("  spaced.pack  ")
                .with_name("   ")
                .with_sound(SoundName::Done, "")
                .with_sound(SoundName::Error, "/e.mp3")
        )
    );
    let listed = attention.packs();
    let pack = listed
        .iter()
        .find(|pack| pack.id == "spaced.pack")
        .expect("the id is trimmed");
    assert_eq!(pack.name, None, "a blank name is no name");
    assert!(attention.activate_pack("spaced.pack"));
    assert!(
        attention.sound_candidates(SoundName::Done).is_empty(),
        "an empty path is not a candidate"
    );
    assert_eq!(
        attention.sound_candidates(SoundName::Error),
        vec![PathBuf::from("/e.mp3")]
    );
}

// ---------------------------------------------------------------------------
// Focus gating
// ---------------------------------------------------------------------------

#[test]
fn attention_focus_gates_the_notification_but_never_the_sound() {
    // Notifications are `when: "blurred"` and audio is `when: "always"`
    // (`notifications.ts:15-16`): a notification for a window you are looking at
    // is noise, a sound is for a user who is not looking at all.
    let cases = [
        (FocusState::Blurred, true, Some(SoundName::Done)),
        (FocusState::Focused, false, Some(SoundName::Done)),
        (FocusState::Unknown, false, Some(SoundName::Done)),
    ];
    for (focus, want_notification, want_sound) in cases {
        let (mut attention, notifier, player) = ready();
        attention.set_focus(focus);
        let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
        assert_eq!(
            outcome.notification, want_notification,
            "{focus:?}: {outcome:?}"
        );
        assert_eq!(
            outcome.sound,
            want_sound.is_some(),
            "{focus:?}: {outcome:?}"
        );
        assert_eq!(
            notifier.count(),
            usize::from(want_notification),
            "{focus:?}"
        );
        assert_eq!(player.count(), 1, "{focus:?}");
    }
}

#[test]
fn attention_a_cue_whose_only_channel_is_gated_reports_the_reason() {
    let notifier = Arc::new(RecordingNotifier::new(true));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound: false,
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone());
    attention.set_focus(FocusState::Focused);

    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert!(!outcome.ok());
    assert_eq!(outcome.skipped, Some(SkipReason::Focused));
    assert_eq!(outcome.skipped.map(SkipReason::as_str), Some("focused"));

    attention.set_focus(FocusState::Unknown);
    assert_eq!(
        attention
            .notify(&AttentionRequest::new(EventClass::Done))
            .skipped,
        Some(SkipReason::FocusUnknown),
        "an unknown focus declines rather than guessing"
    );
}

#[test]
fn attention_a_delivered_half_is_not_reported_as_a_skip() {
    // The notification is gated by focus and the sound is not, so something was
    // delivered; reporting a skip reason next to it would be a lie.
    let (mut attention, notifier, player) = ready();
    attention.set_focus(FocusState::Focused);
    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert!(outcome.ok());
    assert!(!outcome.notification);
    assert!(outcome.sound);
    assert_eq!(outcome.skipped, None, "{outcome:?}");
    assert_eq!((notifier.count(), player.count()), (0, 1));
}

#[test]
fn attention_a_notifier_that_declines_is_not_a_delivered_notification() {
    let notifier = Arc::new(RecordingNotifier::new(false));
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound: false,
        ..ResolvedAttention::default()
    })
    .with_notifier(notifier.clone());
    attention.set_focus(FocusState::Blurred);
    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert_eq!(notifier.count(), 1, "it was asked");
    assert!(!outcome.notification, "and it said no");
    assert!(!outcome.ok());
}

// ---------------------------------------------------------------------------
// Volume
// ---------------------------------------------------------------------------

#[test]
fn attention_volume_is_clamped_with_a_diagnostic_naming_the_value() {
    for (configured, used) in [(1.5_f64, 1.0_f64), (-0.2, 0.0)] {
        let (resolved, diagnostics) = AttentionSettings {
            volume: Some(configured),
            ..AttentionSettings::default()
        }
        .resolve();
        assert_eq!(resolved.volume, used, "{configured}");
        assert_eq!(
            diagnostics,
            vec![AttentionDiagnostic::VolumeClamped { configured, used }],
            "{configured}"
        );
    }

    // NaN is never equal to itself, so this case is asserted structurally.
    let (resolved, diagnostics) = AttentionSettings {
        volume: Some(f64::NAN),
        ..AttentionSettings::default()
    }
    .resolve();
    assert_eq!(
        resolved.volume, 0.0,
        "a volume that is not a number is silence"
    );
    assert!(
        matches!(
            diagnostics.as_slice(),
            [AttentionDiagnostic::VolumeClamped { configured, used }]
                if configured.is_nan() && *used == 0.0
        ),
        "{diagnostics:?}"
    );

    assert_eq!(
        AttentionDiagnostic::VolumeClamped {
            configured: 1.5,
            used: 1.0
        }
        .to_string(),
        "`attention.volume` must be between 0 and 1, but the configuration has `1.5`; using `1`"
    );

    let (resolved, diagnostics) = AttentionSettings {
        volume: Some(0.75),
        ..AttentionSettings::default()
    }
    .resolve();
    assert_eq!(resolved.volume, 0.75);
    assert_eq!(
        diagnostics,
        Vec::new(),
        "an in-range volume is not a finding"
    );
}

#[test]
fn attention_a_load_diagnostic_is_reported_once_with_the_first_cue() {
    let player = Arc::new(RecordingPlayer::new(true));
    let mut attention = Attention::from_settings(&AttentionSettings {
        enabled: Some(true),
        volume: Some(9.0),
        sound_pack: Some("test.pack".to_owned()),
        ..AttentionSettings::default()
    })
    .with_player(player.clone());
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);

    let first = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert_eq!(
        first.diagnostics,
        vec![AttentionDiagnostic::VolumeClamped {
            configured: 9.0,
            used: 1.0
        }]
    );
    let second = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert_eq!(
        second.diagnostics,
        Vec::new(),
        "a load-time finding is not repeated on every cue"
    );
    assert_eq!(
        player.played(),
        vec![
            (PathBuf::from("/packs/test.pack/done.mp3"), 1.0),
            (PathBuf::from("/packs/test.pack/done.mp3"), 1.0),
        ]
    );
}

#[test]
fn attention_a_per_cue_volume_overrides_the_configured_one() {
    let (mut attention, _notifier, player) = ready();
    attention.notify(&AttentionRequest::new(EventClass::Done).with_volume(0.9));
    attention.notify(&AttentionRequest::new(EventClass::Done).with_volume(3.0));
    assert_eq!(
        player
            .played()
            .into_iter()
            .map(|(_, volume)| volume)
            .collect::<Vec<_>>(),
        vec![0.9, 1.0],
        "a per-cue volume is clamped like the configured one"
    );
}

// ---------------------------------------------------------------------------
// Message normalization
// ---------------------------------------------------------------------------

#[test]
fn attention_a_message_is_flattened_stripped_and_capped() {
    assert_eq!(
        normalize_text("Session done", "", MESSAGE_LIMIT),
        "Session done"
    );
    assert_eq!(
        normalize_text("line one \n\n  line two", "", MESSAGE_LIMIT),
        "line one line two",
        "a run of breaks and the whitespace around it become one space"
    );
    assert_eq!(
        normalize_text("\u{1b}[31mred\u{1b}[0m", "", MESSAGE_LIMIT),
        "red",
        "a title carrying model-emitted colour must not reach a tray as escapes"
    );
    assert_eq!(
        normalize_text("a\u{1b}]0;retitle\u{7}b", "", MESSAGE_LIMIT),
        "ab",
        "an OSC sequence is removed with its terminator"
    );
    assert_eq!(
        normalize_text("a\u{1b}]0;retitle\u{1b}\\b", "", MESSAGE_LIMIT),
        "ab",
        "ST terminates a string sequence too"
    );
    assert_eq!(normalize_text("a\u{7}b\u{7f}c", "", MESSAGE_LIMIT), "abc");
    assert_eq!(normalize_text("  padded  ", "", MESSAGE_LIMIT), "padded");
    assert_eq!(
        normalize_text("   ", DEFAULT_TITLE, TITLE_LIMIT),
        DEFAULT_TITLE,
        "an empty title falls back rather than showing nothing"
    );

    let long = "x".repeat(MESSAGE_LIMIT + 50);
    assert_eq!(
        normalize_text(&long, "", MESSAGE_LIMIT).len(),
        MESSAGE_LIMIT
    );
    // Characters, not bytes: truncating a multi-byte title by bytes would either
    // panic or cut a character in half.
    let cjk = "连".repeat(TITLE_LIMIT + 5);
    assert_eq!(
        normalize_text(&cjk, "", TITLE_LIMIT).chars().count(),
        TITLE_LIMIT
    );
}

#[test]
fn attention_an_empty_message_is_a_skip_and_reaches_no_channel() {
    let (mut attention, notifier, player) = ready();
    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done).with_message("  \n "));
    assert_eq!(outcome.skipped, Some(SkipReason::EmptyMessage));
    assert!(!outcome.ok());
    assert_eq!((notifier.count(), player.count()), (0, 0));
}

// ---------------------------------------------------------------------------
// The OSC notifier
// ---------------------------------------------------------------------------

#[test]
fn attention_the_osc_notifier_writes_one_notification_sequence() {
    let notifier = OscNotifier::new(Vec::<u8>::new());
    assert!(notifier.notify("opencode", "Session done"));
    let written = String::from_utf8(notifier.into_inner()).expect("utf-8");
    assert_eq!(written, "\u{1b}]777;notify;opencode;Session done\u{1b}\\");
}

#[test]
fn attention_the_osc_notifier_strips_bytes_that_would_end_the_sequence() {
    // OSC has no escaping mechanism, so a `;` or a terminator inside the text
    // would end the sequence early and spill the rest onto the screen.
    let notifier = OscNotifier::new(Vec::<u8>::new());
    assert!(notifier.notify("a;b\u{1b}c", "d\u{7}e;f"));
    let written = String::from_utf8(notifier.into_inner()).expect("utf-8");
    assert_eq!(written, "\u{1b}]777;notify;abc;def\u{1b}\\");
}

#[test]
fn attention_the_default_channels_are_inert() {
    // Constructing a host must not touch a display server or an audio device,
    // because `cargo test` has neither and neither is a dependency of this crate.
    let mut attention = Attention::new(ResolvedAttention {
        enabled: true,
        sound_pack: "test.pack".to_owned(),
        ..ResolvedAttention::default()
    });
    assert!(attention.register_pack(full_pack("test.pack")));
    attention.set_focus(FocusState::Blurred);
    let outcome = attention.notify(&AttentionRequest::new(EventClass::Done));
    assert!(!outcome.notification, "{outcome:?}");
    assert!(!outcome.sound, "{outcome:?}");
    assert!(!NullNotifier.notify("t", "m"));
    assert!(!SilentPlayer.play(Path::new("/any.mp3"), 1.0));
}

// ---------------------------------------------------------------------------
// Configuration wiring
// ---------------------------------------------------------------------------

#[test]
fn attention_parses_from_the_tui_config_document() {
    let config = TuiConfig::from_json_str(
        r#"{
          "attention": {
            "enabled": true,
            "notifications": false,
            "sound": true,
            "volume": 0.25,
            "sound_pack": "acme.chimes",
            "sounds": { "permission": "/s/permission.mp3", "subagent_done": "/s/sub.mp3" }
          }
        }"#,
    )
    .expect("the document parses");

    let settings = config.attention.expect("the block is present");
    assert_eq!(settings.enabled, Some(true));
    assert_eq!(settings.notifications, Some(false));
    assert_eq!(settings.sound, Some(true));
    assert_eq!(settings.volume, Some(0.25));
    assert_eq!(settings.sound_pack.as_deref(), Some("acme.chimes"));
    assert_eq!(
        settings.sounds,
        BTreeMap::from([
            (SoundName::Permission, PathBuf::from("/s/permission.mp3")),
            (SoundName::SubagentDone, PathBuf::from("/s/sub.mp3")),
        ])
    );

    let (resolved, diagnostics) = settings.resolve();
    assert_eq!(diagnostics, Vec::new());
    assert!(resolved.enabled);
    assert!(!resolved.notifications);
    assert_eq!(resolved.volume, 0.25);
    assert_eq!(resolved.sound_pack, "acme.chimes");
}

#[test]
fn attention_an_absent_block_serializes_to_nothing() {
    // The whole `TuiConfig` must still serialize to `{}` by default, so writing a
    // parsed configuration back out does not invent an `attention` object nobody
    // asked for.
    assert_eq!(
        serde_json::to_string(&TuiConfig::default()).expect("serializable"),
        "{}"
    );
    let config = TuiConfig::from_json_str(r#"{ "attention": { "enabled": true } }"#)
        .expect("the document parses");
    assert_eq!(
        serde_json::to_string(&config).expect("serializable"),
        r#"{"attention":{"enabled":true}}"#
    );
    assert_eq!(
        serde_json::from_str::<TuiConfig>(&serde_json::to_string(&config).expect("serializable"))
            .expect("round trip"),
        config
    );
}

#[test]
fn attention_sound_names_keep_their_upstream_spellings() {
    // These six names are the compatibility surface even though the bytes are not:
    // a user supplying a pack needs to know which slots to fill.
    // `packages/plugin/src/tui.ts:235`.
    let expected = [
        "default",
        "question",
        "permission",
        "error",
        "done",
        "subagent_done",
    ];
    assert_eq!(SoundName::ALL.len(), expected.len());
    for (name, spelling) in SoundName::ALL.into_iter().zip(expected) {
        assert_eq!(name.as_str(), spelling);
        assert_eq!(name.to_string(), spelling);
        assert_eq!(
            serde_json::to_string(&name).expect("serializable"),
            format!("\"{spelling}\"")
        );
        assert_eq!(
            serde_json::from_str::<SoundName>(&format!("\"{spelling}\"")).expect("parses"),
            name
        );
    }
}

// ---------------------------------------------------------------------------
// The asset guard
// ---------------------------------------------------------------------------

#[test]
fn attention_no_audio_asset_is_compiled_into_this_crate() {
    // The standing constraint stated as a guard: the excluded `@opencode-ai/ui`
    // package must not be pulled into the build for four mp3 files, and no audio
    // whose licence cannot be stated may be vendored. Both are the same
    // observable: no audio bytes anywhere under this crate, and nothing embedding
    // any. The floor assertions are mandatory per `.omo/WORKTREE.md` — a scan that
    // walks the wrong directory would otherwise pass vacuously.
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let audio_extensions = ["mp3", "aac", "wav", "ogg", "opus", "flac", "m4a"];

    let mut files = 0usize;
    let mut audio = Vec::new();
    let mut sources = 0usize;
    let mut embedders = Vec::new();
    let mut pending = vec![crate_root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            files += 1;
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if audio_extensions.contains(&extension.as_str()) {
                audio.push(path.display().to_string());
                continue;
            }
            if extension != "rs" {
                continue;
            }
            // This file is excluded because it has to name the macro it forbids;
            // it is also `#[cfg(test)]`, so it cannot reach a shipped binary.
            if path.file_name().and_then(|name| name.to_str()) == Some("attention_tests.rs") {
                continue;
            }
            sources += 1;
            let source = std::fs::read_to_string(&path).expect("readable source");
            for (number, line) in source.lines().enumerate() {
                // A path literal proves nothing — the tests above name files that
                // do not exist. Only `include_bytes!` can put opaque bytes in the
                // binary, which is the thing being forbidden.
                if line.contains("include_bytes!") {
                    embedders.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
    }

    assert!(
        files >= 40,
        "walked only {files} files under {}; the scan is looking in the wrong place and would pass vacuously",
        crate_root.display()
    );
    assert!(
        sources >= 6,
        "read only {sources} Rust sources under {}; the scan would pass vacuously",
        crate_root.display()
    );
    assert!(
        audio.is_empty(),
        "this crate ships audio it cannot state a licence for: {audio:?}"
    );
    assert!(
        embedders.is_empty(),
        "these lines embed bytes into the binary: {embedders:?}"
    );
}
