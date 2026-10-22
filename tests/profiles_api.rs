//! What an app that depends on the library reaches for worker profiles and the
//! two worker settings, without the crate's private items.

use arko::profiles::{self, Author, Cache, Place, Profile, ReadError, Roots};
use arko::workers::{self, ModelChoice};

#[test]
fn an_app_makes_lists_edits_and_deletes_profiles_without_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let roots = Roots::new(temp.path().join("arko"))
        .workspace(temp.path().join("project"))
        .session(temp.path().join("session"));

    let mut profile = Profile::new("3d-judge", "Judges 3D results against a reference image");
    profile.model = Some("sol".into());
    profile.effort = Some(arko::Effort::High);
    profile.cache = Some(Cache::OneHour);
    profile.instructions = "You judge 3D models.".into();
    profile.check().unwrap();
    let saved = profiles::save(&roots, Place::User, &profile, Author::User).unwrap();
    assert!(saved.file.ends_with("profiles/3d-judge.md"));

    let listed = profiles::list(&roots);
    assert_eq!(listed.active.len(), 1);
    assert_eq!(listed.active[0].place, Place::User);
    assert_eq!(listed.active[0].profile.model.as_deref(), Some("sol"));

    // The page edits what it read, and saves it again.
    let mut edited = profiles::read(&roots, "3d-judge").unwrap().profile;
    edited.description = "Judges renders".into();
    edited.isolated = true;
    profiles::save(&roots, Place::User, &edited, Author::User).unwrap();
    assert!(profiles::read(&roots, "3d-judge").unwrap().profile.isolated);

    // The question the agent's save puts to the user, and the rule for the
    // answer.
    let question = profiles::approval_text(&edited, Place::Project);
    assert!(question.contains("3d-judge"));
    assert!(profiles::is_approval("Yes.") && !profiles::is_approval("yes, but low"));

    // The folders to open, nearest first.
    let places: Vec<Place> = profiles::folders(&roots)
        .into_iter()
        .map(|(place, _)| place)
        .collect();
    assert_eq!(places, [Place::Session, Place::Project, Place::User]);

    profiles::delete(&roots, Place::User, "3d-judge").unwrap();
    // The enums can grow, so an app matches them with a wildcard arm.
    match profiles::read(&roots, "3d-judge") {
        Err(ReadError::Missing) => {}
        Err(other) => panic!("expected Missing, got {other}"),
        Ok(found) => panic!("the profile was deleted, got {}", found.profile.name),
    }
    assert_eq!("1H".parse::<Cache>(), Ok(Cache::OneHour));
    assert!("2h".parse::<Cache>().unwrap_err().contains("5m or 1h"));
    assert!(profiles::check_name("Bad Name").is_err());
}

#[test]
fn an_app_names_and_sets_the_worker_settings_with_the_librarys_words() {
    assert_eq!(workers::MODELS_NAME, "Models for workers");
    assert_eq!(workers::DEPTH_NAME, "Worker depth");
    assert_eq!(ModelChoice::Profiles.command(), "/worker-models profiles");
    assert_eq!(arko::ModelChoice::Agent.label(), "Main agent chooses");
    assert_eq!(
        workers::depth_command(workers::DEFAULT_DEPTH),
        "/worker-depth 2"
    );
    assert_eq!(workers::parse_depth("4"), Ok(workers::MAX_DEPTH));
    for command in [
        ModelChoice::Session.command(),
        workers::depth_command(1),
        "/profiles".to_string(),
    ] {
        assert!(arko::commands::find(&command).is_some(), "{command}");
    }
}
