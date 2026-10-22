//! What an app that depends on the library reaches for the monitor wakes
//! setting, without the crate's private items.

use arko::monitors::{COMMAND, NAME, Wakes, parse_wakes};

#[test]
fn an_app_names_and_sets_the_setting_with_the_librarys_words() {
    assert_eq!(NAME, "Monitor wakes");
    assert_eq!(COMMAND, "/monitor-wakes");
    let wakes = parse_wakes("90m").unwrap();
    assert_eq!(wakes.command(), "/monitor-wakes 90m");
    assert_eq!(Wakes::default().command(), "/monitor-wakes 2h");
    assert!(
        arko::commands::find(COMMAND).is_some(),
        "the session takes the command"
    );
}
