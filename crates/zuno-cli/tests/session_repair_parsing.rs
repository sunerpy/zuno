use clap::Parser;
use zuno_cli::Cli;

#[test]
fn session_repair_defaults_to_inspection_and_accepts_explicit_dry_run() {
    for tail in [vec![], vec!["--dry-run"]] {
        let mut args = vec![
            "zuno",
            "session",
            "repair",
            "ses_fixture",
            "--input",
            "msg_saved",
        ];
        args.extend(tail);
        Cli::try_parse_from(args).expect("native repair inspection");
    }
}

#[test]
fn session_repair_apply_requires_a_positive_exact_execution_revision() {
    Cli::try_parse_from([
        "zuno",
        "session",
        "repair",
        "ses_fixture",
        "--input",
        "msg_saved",
        "--apply",
        "--expected-revision",
        "29",
    ])
    .expect("revision-bound repair");
    for tail in [
        vec!["--apply"],
        vec!["--apply", "--expected-revision", "0"],
        vec!["--apply", "--expected-revision", "-1"],
        vec!["--expected-revision", "29"],
        vec!["--apply", "--dry-run", "--expected-revision", "29"],
    ] {
        let mut args = vec![
            "zuno",
            "session",
            "repair",
            "ses_fixture",
            "--input",
            "msg_saved",
        ];
        args.extend(tail);
        assert!(Cli::try_parse_from(args).is_err());
    }
}

#[test]
fn session_repair_requires_one_session_and_one_input() {
    for args in [
        vec!["zuno", "session", "repair", "ses_fixture"],
        vec!["zuno", "session", "repair", "--input", "msg_saved"],
        vec![
            "zuno",
            "session",
            "repair",
            "ses_fixture",
            "--input",
            "msg_saved",
            "--input",
            "msg_other",
        ],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}
